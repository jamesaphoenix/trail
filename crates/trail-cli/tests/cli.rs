//! End-to-end CLI behavior: drive the real binary against a temp tree and
//! assert on JSON output and the loop-relevant exit codes.

use assert_cmd::Command;
use std::fs;
use std::path::Path;

fn trail(root: &Path) -> Command {
    let mut c = Command::cargo_bin("trail").unwrap();
    c.arg("--root").arg(root);
    c
}

fn write(root: &Path, rel: &str, contents: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, contents).unwrap();
}

#[test]
fn init_next_done_until_sweep_complete() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "alpha/a.rs", "fn a() {}");
    write(root, "beta/b.rs", "fn b() {}");

    // init: registers two folders and writes the example config.
    trail(root)
        .arg("init")
        .assert()
        .success()
        .stdout(predicates::str::contains("\"folders\":2"))
        .stdout(predicates::str::contains("\"wrote_example_config\":true"));
    assert!(root.join(".trail.toml.example").exists());

    // first next: bootstraps sweep 1 and leases a folder.
    trail(root)
        .args(["next", "--task", "t", "--agent", "a1"])
        .assert()
        .success()
        .stdout(predicates::str::contains("\"status\":\"ok\""))
        .stdout(predicates::str::contains("\"sweep\":1"));

    // Complete both known folders (one was leased, one still pending: both ok).
    for folder in ["alpha", "beta"] {
        trail(root)
            .args(["done", "--task", "t", "--path", folder, "--agent", "a1"])
            .assert()
            .success()
            .stdout(predicates::str::contains("\"status\":\"done\""));
    }

    // Sweep is now fully covered: next reports sweep-complete with exit code 3.
    trail(root)
        .args(["next", "--task", "t", "--agent", "a1"])
        .assert()
        .code(3)
        .stdout(predicates::str::contains("\"status\":\"sweep-complete\""))
        .stdout(predicates::str::contains("\"covered\":2"));

    // status reflects full coverage.
    trail(root)
        .args(["status", "--task", "t"])
        .assert()
        .success()
        .stdout(predicates::str::contains("\"done\":2"))
        .stdout(predicates::str::contains("\"percent\":100.0"));
}

#[test]
fn second_sweep_remembers_history() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "x/x.rs", "fn x() {}");
    trail(root).arg("init").assert().success();

    // Drain sweep 1.
    trail(root).args(["next", "--task", "t"]).assert().success();
    trail(root)
        .args(["done", "--task", "t", "--path", "x"])
        .assert()
        .success();
    trail(root).args(["next", "--task", "t"]).assert().code(3);

    // Explicit new sweep -> sweep 2, history retained (visits not cleared).
    trail(root)
        .args(["sweep", "new", "--task", "t"])
        .assert()
        .success()
        .stdout(predicates::str::contains("\"sweep\":2"));
    trail(root)
        .args(["next", "--task", "t"])
        .assert()
        .success()
        .stdout(predicates::str::contains("\"sweep\":2"));
}

#[test]
fn none_available_when_all_leased() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "only/only.rs", "fn o() {}");
    trail(root).arg("init").assert().success();

    // a1 leases the single folder.
    trail(root)
        .args(["next", "--task", "t", "--agent", "a1"])
        .assert()
        .success()
        .stdout(predicates::str::contains("\"status\":\"ok\""));

    // a2 finds nothing pending but a live lease: exit code 4.
    trail(root)
        .args(["next", "--task", "t", "--agent", "a2"])
        .assert()
        .code(4)
        .stdout(predicates::str::contains("\"status\":\"none-available\""))
        .stdout(predicates::str::contains("\"leased_outstanding\":1"));
}

#[test]
fn done_with_unknown_path_exits_error() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "real/x.rs", "fn x() {}");
    trail(root).arg("init").assert().success();
    trail(root).args(["next", "--task", "t"]).assert().success();

    // A typo / wrong path is a hard error (exit 1), not silent success.
    trail(root)
        .args(["done", "--task", "t", "--path", "bogus/typo"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("not a work item"));

    // Coverage did not advance.
    trail(root)
        .args(["status", "--task", "t"])
        .assert()
        .success()
        .stdout(predicates::str::contains("\"done\":0"));
}

#[test]
fn empty_repo_next_flags_missing_init() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // No init, no files: completion must be distinguishable from real coverage.
    trail(root)
        .args(["next", "--task", "t"])
        .assert()
        .code(3)
        .stdout(predicates::str::contains("\"total\":0"))
        .stdout(predicates::str::contains("\"note\""));
}

#[test]
fn gc_runs_and_vacuum_flag_is_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "a/a.rs", "fn a() {}");
    trail(root).arg("init").assert().success();
    trail(root)
        .arg("gc")
        .assert()
        .success()
        .stdout(predicates::str::contains("\"reclaimed_leases\""));
    trail(root).args(["gc", "--vacuum"]).assert().success();
}

#[test]
fn bad_config_ttl_exits_error() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "a/a.rs", "fn a() {}");
    std::fs::write(root.join(".trail.toml"), "[lease]\nttl_secs = -100\n").unwrap();
    trail(root)
        .args(["next", "--task", "t"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("ttl_secs"));
}

#[test]
fn done_without_a_sweep_errors() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "a/a.rs", "fn a() {}");
    trail(root).arg("init").assert().success();
    // Folders are registered but no sweep is open yet (no `next`): a `done` has
    // no work item to land on, so it errors rather than silently succeeding.
    trail(root)
        .args(["done", "--task", "t", "--path", "a"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("not a work item"));
}

#[test]
fn rescan_new_sweep_picks_up_added_folder() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "a/a.rs", "fn a() {}");
    trail(root).arg("init").assert().success();
    // Drain sweep 1 (one folder).
    trail(root).args(["next", "--task", "t"]).assert().success();
    trail(root)
        .args(["done", "--task", "t", "--path", "a"])
        .assert()
        .success();
    trail(root).args(["next", "--task", "t"]).assert().code(3);
    // The tree grows; rescan into a new sweep sees both folders.
    write(root, "b/b.rs", "fn b() {}");
    trail(root)
        .args(["sweep", "new", "--task", "t", "--rescan"])
        .assert()
        .success()
        .stdout(predicates::str::contains("\"total\":2"));
}

#[test]
fn completions_generate_for_each_shell() {
    // Completions must work with no repo/config present.
    let dir = tempfile::tempdir().unwrap();
    for (shell, needle) in [
        ("bash", "_trail"),
        ("zsh", "#compdef trail"),
        ("fish", "complete -c trail"),
    ] {
        trail(dir.path())
            .args(["completions", shell])
            .assert()
            .success()
            .stdout(predicates::str::contains(needle));
    }
}

#[test]
fn completions_survive_a_closed_pipe() {
    // Mirrors `trail completions bash | true`: close the read end before the
    // child writes, so the write hits EPIPE. Must not panic (exit 101).
    use std::process::{Command, Stdio};
    let bin = assert_cmd::cargo::cargo_bin("trail");
    let mut child = Command::new(bin)
        .args(["completions", "bash"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Drop the read end immediately without reading any of it.
    drop(child.stdout.take());
    let out = child.wait_with_output().unwrap();
    assert_ne!(
        out.status.code(),
        Some(101),
        "completions panicked on a closed pipe: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "expected clean exit on closed pipe");
}

#[test]
fn sweep_new_while_active_errors() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "a/a.rs", "fn a() {}");
    write(root, "b/b.rs", "fn b() {}");
    trail(root).arg("init").assert().success();
    // Open sweep 1 and leave it active.
    trail(root).args(["next", "--task", "t"]).assert().success();
    trail(root)
        .args(["sweep", "new", "--task", "t"])
        .assert()
        .code(1)
        .stderr(predicates::str::contains("still active"));
}
