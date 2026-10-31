//! End-to-end explanation tests: facade packages and the `keel explain` CLI.

use keel::{ExplainOptions, Index};
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
fn facade_explains_symbol_file_and_module() {
    let dir = fixture_project();
    let mut index = Index::open_in_memory().unwrap();
    index.index_path(dir.path()).unwrap();
    let options = ExplainOptions {
        source_root: Some(dir.path().to_path_buf()),
    };

    let symbol = index.explain_symbol("serve", None, &options).unwrap();
    assert!(symbol.definition.is_some());
    assert!(symbol.excerpt.as_deref().unwrap_or("").contains("def serve"));
    assert!(!symbol.callers.is_empty());
    assert!(!symbol.generated);

    let file = index.explain_file("src/svc.py").unwrap();
    assert!(file.file.is_some());
    assert_eq!(file.file.as_ref().unwrap().symbols_total, 2);

    let defs = index.definition("serve").unwrap();
    let module = defs[0].module_path.clone();
    let explained = index.explain_module(&module).unwrap();
    assert!(explained.module.is_some());
}

#[test]
fn cli_explain_forms_json_and_conflicts() {
    let dir = fixture_project();
    let root = dir.path();
    let out = Command::new(keel_bin())
        .args(["index", "."])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success());

    // Symbol form: header + definition + excerpt.
    let out = Command::new(keel_bin())
        .args(["explain", "serve"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("# keel explain: serve (symbol)"), "{stdout}");
    assert!(stdout.contains("definition: src/svc.py:1"), "{stdout}");
    assert!(stdout.contains("def serve(x):"), "{stdout}");
    assert!(stdout.contains("callers"), "{stdout}");

    // File form as JSON.
    let out = Command::new(keel_bin())
        .args(["--json", "explain", "--file", "src/svc.py"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success());
    let value: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert_eq!(value["target_kind"], "file");
    assert_eq!(value["file"]["symbols_total"], 2);
    assert_eq!(value["generated"], false);

    // Conflicting targets fail loudly (nonzero exit).
    let out = Command::new(keel_bin())
        .args(["explain", "serve", "--file", "src/svc.py"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("exactly one"), "{stderr}");
}
