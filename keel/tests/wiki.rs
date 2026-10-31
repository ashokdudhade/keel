//! End-to-end wiki tests: facade reports and the `keel wiki` CLI.

use keel::{Index, PageState};
use std::fs;
use std::process::Command;

fn keel_bin() -> &'static str {
    env!("CARGO_BIN_EXE_keel")
}

fn fixture_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/svc.py"),
        "def serve(x):\n    return x\n\ndef run():\n    return serve(1)\n",
    )
    .unwrap();
    dir
}

#[test]
fn facade_generate_update_status_cycle() {
    let dir = fixture_project();
    let mut index = Index::open_in_memory().unwrap();
    index.index_path(dir.path()).unwrap();
    let out = dir.path().join("docs").join("keel");

    let report = index.wiki_generate(&out).unwrap();
    assert!(!report.written.is_empty());
    assert!(out.join("index.md").exists());
    assert!(out.join("modules/src.md").exists());

    let status = index.wiki_status(&out).unwrap();
    assert!(status.pages.iter().all(|p| p.state == PageState::Current));

    // Edit + reindex: status flags stale, update rewrites, status clears.
    fs::write(
        dir.path().join("src/svc.py"),
        "def serve(x):\n    return x\n\ndef run():\n    return serve(2)\n",
    )
    .unwrap();
    index.index_path(dir.path()).unwrap();
    let status = index.wiki_status(&out).unwrap();
    assert!(status.pages.iter().any(|p| p.state == PageState::Stale));
    let report = index.wiki_update(&out).unwrap();
    assert!(!report.written.is_empty());
    let status = index.wiki_status(&out).unwrap();
    assert!(status.pages.iter().all(|p| p.state == PageState::Current));
}

#[test]
fn cli_wiki_generate_update_status_and_json() {
    let dir = fixture_project();
    let root = dir.path();
    let out = Command::new(keel_bin())
        .args(["index", "."])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success());

    let out = Command::new(keel_bin())
        .args(["wiki", "generate"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("wrote index.md"), "{stdout}");
    assert!(root.join("docs/keel/modules/src.md").exists());

    // Human note survives the update cycle.
    fs::write(
        root.join("docs/keel/modules/src.md"),
        format!(
            "{}\n## Team notes\n\nOwned by payments.\n",
            fs::read_to_string(root.join("docs/keel/modules/src.md")).unwrap()
        ),
    )
    .unwrap();
    fs::write(
        root.join("src/svc.py"),
        "def serve(x):\n    return x\n\ndef run():\n    return serve(3)\n",
    )
    .unwrap();
    let out = Command::new(keel_bin())
        .args(["wiki", "status"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("stale"), "{stdout}");

    let out = Command::new(keel_bin())
        .args(["wiki", "update"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success());
    let merged = fs::read_to_string(root.join("docs/keel/modules/src.md")).unwrap();
    assert!(merged.contains("Owned by payments."));
    assert!(merged.contains("keel:generated:start"));

    // JSON status parses with the full envelope.
    let out = Command::new(keel_bin())
        .args(["--json", "wiki", "status"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success());
    let value: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert!(value["pages"].as_array().unwrap().iter().all(|p| p["state"] == "current"));
}
