//! End-to-end architecture-rules tests: facade checks and the `keel rules` CLI.

use keel::{ForbidRule, Index, Layer, Rules};
use std::fs;
use std::process::Command;

fn keel_bin() -> &'static str {
    env!("CARGO_BIN_EXE_keel")
}

fn fixture_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("pkg")).unwrap();
    fs::write(
        root.join("pkg/api.py"),
        "import pkg.db\n\n\ndef handle():\n    pkg.db.query()\n",
    )
    .unwrap();
    fs::write(root.join("pkg/db.py"), "def query():\n    pass\n").unwrap();
    fs::write(
        root.join("pkg/core.py"),
        "import pkg.db\n\n\ndef work():\n    pkg.db.query()\n",
    )
    .unwrap();
    dir
}

fn indexed(root: &std::path::Path) -> Index {
    let mut index = Index::open_in_memory().unwrap();
    index.index_path(root).unwrap();
    index
}

fn forbid(from: &str, to: &str) -> Rules {
    Rules {
        forbid: vec![ForbidRule {
            from: from.to_string(),
            to: to.to_string(),
            reason: None,
        }],
        layers: vec![],
    }
}

#[test]
fn forbid_rule_fires_on_real_edge_with_evidence() {
    let dir = fixture_project();
    let index = indexed(dir.path());
    let report = index
        .rules_check(&forbid("pkg.api", "pkg.db"))
        .unwrap();
    assert_eq!(report.violations.len(), 1);
    let hit = &report.violations[0];
    assert_eq!(hit.from_module, "pkg.api");
    assert_eq!(hit.to_module, "pkg.db");
    assert!(hit.rule.contains("forbid"), "{}", hit.rule);
    assert_eq!(
        hit.via.as_ref().unwrap().as_os_str(),
        "pkg/api.py",
        "import evidence names the importing file"
    );
    assert!(report.notes.is_empty(), "{:?}", report.notes);
}

#[test]
fn forbid_rule_passes_when_no_edge_exists() {
    let dir = fixture_project();
    let index = indexed(dir.path());
    let report = index.rules_check(&forbid("pkg.db", "pkg.api")).unwrap();
    assert!(report.is_clean());
    assert_eq!(report.forbid_checked, 1);
}

#[test]
fn forbid_prefix_covers_submodules_not_siblings() {
    let dir = fixture_project();
    let index = indexed(dir.path());
    // `pkg` covers pkg.api and pkg.core, which both import pkg.db.
    let report = index.rules_check(&forbid("pkg", "pkg.db")).unwrap();
    assert_eq!(report.violations.len(), 2);
    // `pkg.a` must not prefix-match `pkg.api`.
    let report = index.rules_check(&forbid("pkg.a", "pkg.db")).unwrap();
    assert!(report.is_clean());
    assert!(
        report.notes.iter().any(|n| n.contains("pkg.a")),
        "{:?}",
        report.notes
    );
}

#[test]
fn layers_allow_downward_and_flag_upward_edges() {
    let dir = fixture_project();
    let index = indexed(dir.path());
    let layered = |order: &[&str]| Rules {
        forbid: vec![],
        layers: order
            .iter()
            .map(|name| Layer {
                name: (*name).to_string(),
                modules: vec![format!("pkg.{name}")],
            })
            .collect(),
    };
    // api above db: the api -> db edge points down, clean.
    let clean = index.rules_check(&layered(&["api", "db"])).unwrap();
    assert!(clean.is_clean(), "{:?}", clean.violations);
    assert_eq!(clean.layers_checked, 2);
    // db above api: the same edge now points up, violation.
    let dirty = index.rules_check(&layered(&["db", "api"])).unwrap();
    assert_eq!(dirty.violations.len(), 1);
    assert_eq!(dirty.violations[0].from_module, "pkg.api");
    assert_eq!(dirty.violations[0].to_module, "pkg.db");
}

#[test]
fn unindexed_constrained_modules_note_instead_of_silently_passing() {
    let dir = fixture_project();
    let index = indexed(dir.path());
    let report = index.rules_check(&forbid("pkg.nope", "pkg.db")).unwrap();
    assert!(report.is_clean());
    assert!(
        report.notes.iter().any(|n| n.contains("pkg.nope")),
        "{:?}",
        report.notes
    );
}

#[test]
fn reason_is_echoed_on_violations() {
    let dir = fixture_project();
    let index = indexed(dir.path());
    let rules = Rules {
        forbid: vec![ForbidRule {
            from: "pkg.api".to_string(),
            to: "pkg.db".to_string(),
            reason: Some("api must go through core".to_string()),
        }],
        layers: vec![],
    };
    let report = index.rules_check(&rules).unwrap();
    assert_eq!(
        report.violations[0].reason.as_deref(),
        Some("api must go through core")
    );
}

#[test]
fn rules_match_resolved_dependency_ids() {
    // `from pkg import db` resolves to the external `pkg::db` edge (see
    // `keel dependencies`), not the dotted `pkg.db` module: rules match
    // resolved ids exactly, never by spelling guess.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("pkg")).unwrap();
    fs::write(
        root.join("pkg/api.py"),
        "from pkg import db\n\n\ndef handle():\n    db.query()\n",
    )
    .unwrap();
    fs::write(root.join("pkg/db.py"), "def query():\n    pass\n").unwrap();
    let index = indexed(root);
    let dotted = index.rules_check(&forbid("pkg.api", "pkg.db")).unwrap();
    assert!(dotted.is_clean(), "{:?}", dotted.violations);
    let resolved = index.rules_check(&forbid("pkg.api", "pkg::db")).unwrap();
    assert_eq!(resolved.violations.len(), 1);
}

fn write_rules(root: &std::path::Path, body: &str) -> std::path::PathBuf {
    let path = root.join("rules.json");
    fs::write(&path, body).unwrap();
    path
}

#[test]
fn cli_exits_1_on_violations_and_0_when_clean() {
    let dir = fixture_project();
    let root = dir.path();
    let dirty = write_rules(
        root,
        r#"{"forbid": [{"from": "pkg.api", "to": "pkg.db"}]}"#,
    );
    let out = Command::new(keel_bin())
        .args(["rules", dirty.to_str().unwrap()])
        .current_dir(root)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("pkg.api -> pkg.db"), "{text}");
    assert!(text.contains("via: pkg/api.py"), "{text}");

    let clean = write_rules(
        root,
        r#"{"forbid": [{"from": "pkg.db", "to": "pkg.api"}]}"#,
    );
    let out = Command::new(keel_bin())
        .args(["rules", clean.to_str().unwrap()])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("No rule violations"), "{text}");
}

#[test]
fn cli_json_renders_report_shape() {
    let dir = fixture_project();
    let root = dir.path();
    let rules = write_rules(
        root,
        r#"{"forbid": [{"from": "pkg.api", "to": "pkg.db"}]}"#,
    );
    let out = Command::new(keel_bin())
        .args(["--json", "rules", rules.to_str().unwrap()])
        .current_dir(root)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let text = String::from_utf8(out.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(parsed["violations"].as_array().unwrap().len(), 1);
    assert_eq!(parsed["violations"][0]["from_module"], "pkg.api");
    assert_eq!(parsed["forbid_checked"], 1);
}

#[test]
fn cli_errors_honestly_on_missing_and_malformed_files() {
    let dir = fixture_project();
    let root = dir.path();
    let out = Command::new(keel_bin())
        .args(["rules", "does-not-exist.json"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("does-not-exist.json"), "{err}");

    let bad = write_rules(root, "{ not json");
    let out = Command::new(keel_bin())
        .args(["rules", bad.to_str().unwrap()])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("invalid rules file"), "{err}");
}

#[test]
fn cli_discovers_dot_keel_rules_json() {
    let dir = fixture_project();
    let root = dir.path();
    fs::create_dir_all(root.join(".keel")).unwrap();
    fs::write(
        root.join(".keel/rules.json"),
        r#"{"forbid": [{"from": "pkg.db", "to": "pkg.api"}]}"#,
    )
    .unwrap();
    let out = Command::new(keel_bin())
        .args(["rules"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("No rule violations"), "{text}");
}
