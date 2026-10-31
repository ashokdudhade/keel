//! End-to-end knowledge tests: records link to indexed file revisions
//! through the public [`Index`] API.

use keel::db::queries;
use keel::{
    Index, KnowledgeEvidence, KnowledgeOrigin, KnowledgeRecord, KnowledgeStatus, KnowledgeType,
};
use std::fs;
use std::process::Command;

fn keel_bin() -> &'static str {
    env!("CARGO_BIN_EXE_keel")
}

fn indexed_project() -> (tempfile::TempDir, Index, String) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(
        dir.path().join("src/auth.rs"),
        "pub fn validate_token() {}\n",
    )
    .unwrap();
    // On-disk so the test can read the stored revision pin independently.
    let db = dir.path().join("index.db");
    let mut index = Index::open(&db).unwrap();
    let stats = index.index_path(dir.path()).unwrap();
    assert_eq!(stats.indexed, 1);
    assert_eq!(stats.errors, 0);
    // Real content hash straight from the index: the revision pin.
    let conn = rusqlite::Connection::open(&db).unwrap();
    let hash = queries::existing_hashes(&conn)
        .unwrap()
        .remove("src/auth.rs")
        .unwrap();
    (dir, index, hash)
}

#[test]
fn knowledge_record_links_to_indexed_symbol_revision() {
    let (_dir, mut index, hash) = indexed_project();
    assert_eq!(index.definition("validate_token").unwrap().len(), 1);

    let mut record = KnowledgeRecord::new(
        "auth.jwt.validation",
        KnowledgeType::Fact,
        "JWT validation",
        "JWT validation occurs in validate_token",
        KnowledgeOrigin::Agent,
    );
    record.evidence.push(KnowledgeEvidence {
        source_path: "src/auth.rs".to_string(),
        symbol: Some("validate_token".to_string()),
        source_hash: Some(hash.clone()),
        commit_hash: None,
    });
    index.knowledge_put(&record).unwrap();

    let got = index
        .knowledge_get("auth.jwt.validation")
        .unwrap()
        .expect("stored record");
    assert_eq!(got.status, KnowledgeStatus::Unverified);
    assert_eq!(got.evidence.len(), 1);
    assert_eq!(got.evidence[0].source_hash.as_deref(), Some(hash.as_str()));
    assert_eq!(index.knowledge_list().unwrap().len(), 1);
}

#[test]
fn facade_enforces_human_provenance() {
    let (_dir, mut index, _hash) = indexed_project();
    index
        .knowledge_put(&KnowledgeRecord::new(
            "adr.001",
            KnowledgeType::Decision,
            "Use JWT",
            "We use JWT for auth",
            KnowledgeOrigin::Human,
        ))
        .unwrap();
    let err = index
        .knowledge_put(&KnowledgeRecord::new(
            "adr.001",
            KnowledgeType::Decision,
            "Use sessions",
            "Actually use sessions",
            KnowledgeOrigin::Agent,
        ))
        .unwrap_err();
    assert!(
        matches!(err, keel::KeelError::KnowledgeProvenance { .. }),
        "unexpected error: {err}"
    );
}

fn run_knowledge(args: &[&str], root: &std::path::Path) -> (bool, String) {
    let out = Command::new(keel_bin())
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn pinned_supported(db: &std::path::Path, id: &str) {
    let mut index = Index::open(db).unwrap();
    let conn = rusqlite::Connection::open(db).unwrap();
    let hash = queries::existing_hashes(&conn)
        .unwrap()
        .remove("src/a.rs")
        .unwrap();
    drop(conn);
    let mut record = KnowledgeRecord::new(
        id,
        KnowledgeType::Fact,
        "Alpha exists",
        "Function alpha is defined in src/a.rs",
        KnowledgeOrigin::Agent,
    );
    record.status = KnowledgeStatus::Supported;
    record.evidence.push(KnowledgeEvidence {
        source_path: "src/a.rs".to_string(),
        symbol: Some("alpha".to_string()),
        source_hash: Some(hash),
        commit_hash: None,
    });
    index.knowledge_put(&record).unwrap();
}

#[test]
fn cli_status_validate_cycle_through_binary() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn alpha() {}\n").unwrap();
    assert!(run_knowledge(&["index", "."], root).0);
    pinned_supported(&root.join(".keel/index.db"), "k1");

    // Fresh checkout: status reports fresh, exit 0.
    let (ok, stdout) = run_knowledge(&["knowledge", "status"], root);
    assert!(ok, "status must exit 0");
    assert!(
        stdout.contains("k1:") && stdout.contains("supported") && stdout.contains("fresh"),
        "{stdout}"
    );

    // Edit the source: the next status auto-indexes and reports drift.
    fs::write(root.join("src/a.rs"), "pub fn alpha() {}\n// note\n").unwrap();
    let (ok, stdout) = run_knowledge(&["knowledge", "status"], root);
    assert!(ok, "drifted status must still exit 0");
    assert!(stdout.contains("k1:") && stdout.contains("drifted"), "{stdout}");

    // Validate heals the record against the new revision.
    let (ok, stdout) = run_knowledge(&["knowledge", "validate", "k1"], root);
    assert!(ok);
    assert!(stdout.contains("Validated k1"), "{stdout}");

    // JSON status parses and agrees.
    let (ok, stdout) = run_knowledge(&["--json", "knowledge", "status"], root);
    assert!(ok);
    let value: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(value["records"][0]["id"], "k1");
    assert_eq!(value["records"][0]["fresh"], true);
}

#[test]
fn cli_empty_store_and_unknown_id() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn alpha() {}\n").unwrap();
    assert!(run_knowledge(&["index", "."], root).0);

    let (ok, stdout) = run_knowledge(&["knowledge", "status"], root);
    assert!(ok);
    assert!(stdout.contains("No knowledge records."), "{stdout}");

    let (ok, stdout) = run_knowledge(&["knowledge", "validate", "nope"], root);
    assert!(ok);
    assert!(stdout.contains("Unknown knowledge id: nope"), "{stdout}");
}
