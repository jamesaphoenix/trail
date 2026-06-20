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
