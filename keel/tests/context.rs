//! End-to-end task-context tests: facade packages and the `keel context` CLI.

use keel::{
    ContextItem, ContextOptions, Index, KnowledgeOrigin, KnowledgeRecord, KnowledgeType,
};
use std::fs;
use std::process::Command;

fn keel_bin() -> &'static str {
    env!("CARGO_BIN_EXE_keel")
}

fn fixture_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("tests")).unwrap();
    fs::write(
        root.join("src/payments.py"),
        "def charge(amount):\n    return amount\n\ndef refund(amount):\n    return -amount\n",
    )
    .unwrap();
    fs::write(
        root.join("tests/test_payments.py"),
        "from src.payments import refund\n\ndef test_refund():\n    assert refund(5) == -5\n",
    )
    .unwrap();
    dir
}

#[test]
fn facade_package_is_ranked_cited_and_deterministic() {
    let dir = fixture_project();
    let mut index = Index::open_in_memory().unwrap();
    index.index_path(dir.path()).unwrap();
    let mut record = KnowledgeRecord::new(
        "payments.refunds",
        KnowledgeType::Fact,
        "Refunds",
        "Refunds negate the charge amount",
        KnowledgeOrigin::Agent,
    );
    record.status = keel::KnowledgeStatus::Supported;
    index.knowledge_put(&record).unwrap();

    let options = ContextOptions {
        source_root: Some(dir.path().to_path_buf()),
        ..ContextOptions::default()
    };
    let a = index.task_context("Add refund support to payments", &options).unwrap();
    let b = index.task_context("Add refund support to payments", &options).unwrap();
    assert_eq!(
        serde_json::to_string(&a).unwrap(),
        serde_json::to_string(&b).unwrap(),
        "identical inputs must yield identical packages"
    );
    assert!(a.estimated_tokens <= a.token_budget);
    assert_eq!(a.omitted, 0);

    // Symbol top hit with citation + excerpt, knowledge + test alongside.
    assert!(matches!(&a.items[0], ContextItem::Symbol { name, .. } if name == "refund"));
    assert!(a.items.iter().any(|i| matches!(
        i,
        ContextItem::Symbol { text: Some(t), .. } if t.contains("def refund")
    )));
    assert!(a.items.iter().any(|i| matches!(
        i,
        ContextItem::Knowledge { record, .. } if record.id == "payments.refunds"
    )));
    assert!(a.items.iter().any(|i| matches!(i, ContextItem::Test { .. })));

    // Toggles narrow the kinds.
    let no_tests = ContextOptions {
        include_tests: false,
        source_root: Some(dir.path().to_path_buf()),
        ..ContextOptions::default()
    };
    let package = index.task_context("refund", &no_tests).unwrap();
    assert!(package.items.iter().all(|i| !matches!(i, ContextItem::Test { .. })));
    let no_kb = ContextOptions {
        include_knowledge: false,
        source_root: Some(dir.path().to_path_buf()),
        ..ContextOptions::default()
    };
    let package = index.task_context("refund", &no_kb).unwrap();
    assert!(package.items.iter().all(|i| !matches!(i, ContextItem::Knowledge { .. })));
}

#[test]
fn cli_context_text_and_json() {
    let dir = fixture_project();
    let root = dir.path();
    let out = Command::new(keel_bin())
        .args(["index", "."])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success());

    let out = Command::new(keel_bin())
        .args(["context", "Add refund support to payments"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("# keel context:"), "{stdout}");
    assert!(stdout.contains("src/payments.py:4"), "{stdout}");
    assert!(stdout.contains("def refund(amount):"), "{stdout}");

    // Tight budget cuts the tail and says so.
    let out = Command::new(keel_bin())
        .args(["context", "Add refund support to payments", "--budget", "100"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("omitted"), "{stdout}");

    // JSON parses with the full envelope.
    let out = Command::new(keel_bin())
        .args(["--json", "context", "refund"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["task"], "refund");
    assert!(value["estimated_tokens"].as_u64().unwrap() <= value["token_budget"].as_u64().unwrap());
    assert!(!value["items"].as_array().unwrap().is_empty());

    // Opt-outs work end to end.
    let out = Command::new(keel_bin())
        .args(["--json", "context", "refund", "--no-tests", "--no-knowledge"])
        .current_dir(root)
        .output()
        .unwrap();
    assert!(out.status.success());
    let value: serde_json::Value =
        serde_json::from_str(&String::from_utf8_lossy(&out.stdout)).unwrap();
    assert!(value["items"].as_array().unwrap().iter().all(|i| {
        i["kind"] != "test" && i["kind"] != "knowledge"
    }));
}
