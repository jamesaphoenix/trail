//! Folder enumeration + static signals via the `ignore` crate (ripgrep's
//! walker). We get `.gitignore`, `.ignore`, hidden-file skipping, and language
//! defaults for free; config only layers extra include/exclude globs on top.

use crate::config::Config;
use crate::error::{Error, Result};
use crate::model::FolderStat;
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;
use std::collections::BTreeMap;
use std::path::Path;

/// Result of scanning a tree.
pub struct WalkOutcome {
    /// Folders meeting `min_files`, sorted by path.
    pub folders: Vec<FolderStat>,
    /// Directories encountered that fell below `min_files`.
    pub excluded: usize,
}

/// Enumerate folders under `root`, attributing each file's count and size to
/// its immediate parent directory.
pub fn scan(root: &Path, cfg: &Config) -> Result<WalkOutcome> {
    let overrides = build_overrides(root, cfg)?;

    let mut builder = WalkBuilder::new(root);
    builder
        .hidden(true) // skip dotfiles/dirs (.git, .trail, .DS_Store, ...)
        .git_ignore(cfg.scan.respect_gitignore)
        .git_global(cfg.scan.respect_gitignore)
        .git_exclude(cfg.scan.respect_gitignore)
        .ignore(cfg.scan.respect_gitignore)
        .parents(cfg.scan.respect_gitignore)
        .require_git(false) // honor .gitignore even outside a git repo (tests)
        .follow_links(false)
        .overrides(overrides);

    // path -> (direct file count, summed size in bytes)
    let mut acc: BTreeMap<String, (i64, i64)> = BTreeMap::new();

    for result in builder.build() {
        let entry = match result {
            Ok(e) => e,
            Err(_) => continue, // unreadable entry: skip rather than abort the scan
        };
        let is_file = entry.file_type().map(|t| t.is_file()).unwrap_or(false);
        if !is_file {
            continue;
        }
        let path = entry.path();
        let size = entry.metadata().map(|m| m.len() as i64).unwrap_or(0);
        let parent = path.parent().unwrap_or(root);
        let rel = rel_path(root, parent);
        let slot = acc.entry(rel).or_insert((0, 0));
        slot.0 += 1;
        slot.1 += size;
    }

    let min = cfg.scan.min_files as i64;
    let mut folders = Vec::new();
    let mut excluded = 0usize;
    for (path, (file_count, size_bytes)) in acc {
        if file_count < min {
            excluded += 1;
            continue;
        }
        folders.push(FolderStat {
            path,
            file_count,
            size_bytes,
            churn: 0,
        });
    }

    #[cfg(feature = "churn")]
    apply_churn(root, &mut folders)?;

    folders.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(WalkOutcome { folders, excluded })
}

/// Translate config include/exclude globs into an `ignore` override matcher.
///
/// In `ignore`'s overrides a plain glob is a *whitelist* (include only) and a
/// `!`-prefixed glob is an *exclude*. The universal include `**/*` is treated
/// as "no whitelist" so it does not accidentally narrow the walk.
fn build_overrides(root: &Path, cfg: &Config) -> Result<ignore::overrides::Override> {
    let mut ob = OverrideBuilder::new(root);
    if !cfg.include_is_catch_all() {
        for g in &cfg.scan.include {
            ob.add(g)
                .map_err(|e| Error::Walk(format!("bad include glob {g:?}: {e}")))?;
        }
    }
    for g in &cfg.scan.exclude {
        ob.add(&format!("!{g}"))
            .map_err(|e| Error::Walk(format!("bad exclude glob {g:?}: {e}")))?;
    }
    ob.build()
        .map_err(|e| Error::Walk(format!("override build: {e}")))
}

/// Normalize a user-supplied folder path so it matches the stored form:
/// forward slashes, no leading `./`, no trailing slash, and the empty string
/// (or `.`) maps to ".". This lets `done`/`skip` accept native or
/// backslash paths and still match what `init` recorded.
pub fn normalize_rel(path: &str) -> String {
    let s = path.replace('\\', "/");
    let s = s.strip_prefix("./").unwrap_or(&s);
    let s = s.trim_end_matches('/');
    if s.is_empty() {
        ".".to_string()
    } else {
        s.to_string()
    }
}

/// `p` relative to `root`, using forward slashes; the root itself is ".".
fn rel_path(root: &Path, p: &Path) -> String {
    match p.strip_prefix(root) {
        Ok(rel) => {
            let s = rel.to_string_lossy().replace('\\', "/");
            if s.is_empty() {
                ".".to_string()
            } else {
                s
            }
        }
        Err(_) => p.to_string_lossy().replace('\\', "/"),
    }
}

/// Compute a simple per-folder git churn score: the number of file changes in
/// the last `MAX_COMMITS` commits attributed to each folder. Leaves churn at 0
/// when `root` is not inside a git repo.
#[cfg(feature = "churn")]
fn apply_churn(root: &Path, folders: &mut [FolderStat]) -> Result<()> {
    use std::collections::HashMap;
    const MAX_COMMITS: usize = 500;

    let repo = match git2::Repository::discover(root) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    let workdir = repo.workdir().unwrap_or(root).to_path_buf();
    let mut revwalk = repo.revwalk().map_err(|e| Error::Walk(e.to_string()))?;
    if revwalk.push_head().is_err() {
        return Ok(());
    }

    let mut counts: HashMap<String, i64> = HashMap::new();
    for (i, oid) in revwalk.flatten().enumerate() {
        if i >= MAX_COMMITS {
            break;
        }
        let commit = match repo.find_commit(oid) {
            Ok(c) => c,
            Err(_) => continue,
        };
        let tree = match commit.tree() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let parent_tree = commit.parent(0).ok().and_then(|p| p.tree().ok());
        let diff = match repo.diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let _ = diff.foreach(
            &mut |delta, _| {
                if let Some(p) = delta.new_file().path() {
                    if let Some(parent) = workdir.join(p).parent() {
                        let rel = rel_path(root, parent);
                        *counts.entry(rel).or_insert(0) += 1;
                    }
                }
                true
            },
            None,
            None,
            None,
        );
    }

    for f in folders.iter_mut() {
        if let Some(c) = counts.get(&f.path) {
            f.churn = *c;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn write(path: &Path, contents: &str) {
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn scans_folders_and_counts_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("src/a.rs"), "fn a() {}");
        write(&root.join("src/b.rs"), "fn b() {}");
        write(&root.join("docs/readme.md"), "hi");
        write(&root.join("top.txt"), "x");

        let cfg = Config::default();
        let out = scan(root, &cfg).unwrap();
        let paths: Vec<_> = out.folders.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"src"));
        assert!(paths.contains(&"docs"));
        assert!(paths.contains(&".")); // top.txt lives at the root

        let src = out.folders.iter().find(|f| f.path == "src").unwrap();
        assert_eq!(src.file_count, 2);
        assert!(src.size_bytes > 0);
    }

    #[test]
    fn respects_gitignore_and_excludes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join(".gitignore"), "ignored/\n");
        write(&root.join("ignored/secret.rs"), "x");
        write(&root.join("kept/keep.rs"), "x");
        write(&root.join("migrations/0001.sql"), "x");

        let mut cfg = Config::default();
        cfg.scan.exclude = vec!["**/migrations/**".to_string()];
        let out = scan(root, &cfg).unwrap();
        let paths: Vec<_> = out.folders.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"kept"));
        assert!(!paths.contains(&"ignored"), "gitignored dir excluded");
        assert!(!paths.contains(&"migrations"), "config-excluded dir gone");
    }

    #[test]
    fn min_files_filters_thin_folders() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(&root.join("thin/only.rs"), "x");
        write(&root.join("fat/a.rs"), "x");
        write(&root.join("fat/b.rs"), "x");

        let mut cfg = Config::default();
        cfg.scan.min_files = 2;
        let out = scan(root, &cfg).unwrap();
        let paths: Vec<_> = out.folders.iter().map(|f| f.path.as_str()).collect();
        assert!(paths.contains(&"fat"));
        assert!(!paths.contains(&"thin"));
        assert!(out.excluded >= 1);
    }
}
