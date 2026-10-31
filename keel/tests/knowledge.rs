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

fn agent_record(id: &str, agent: Option<&str>, origin: KnowledgeOrigin) -> KnowledgeRecord {
    let mut record = KnowledgeRecord::new(
        id,
        KnowledgeType::Fact,
        format!("title {id}"),
        format!("content mentions jwt for {id}"),
        origin,
    );
    record.agent = agent.map(str::to_string);
    record
}

#[test]
fn agent_identity_roundtrips_and_filters() {
    let (_dir, mut index, _hash) = indexed_project();
    index
        .knowledge_put(&agent_record("a.one", Some("codex"), KnowledgeOrigin::Agent))
        .unwrap();
    index
        .knowledge_put(&agent_record("a.two", Some("claude-code"), KnowledgeOrigin::Agent))
        .unwrap();
    index
        .knowledge_put(&agent_record("a.three", None, KnowledgeOrigin::Human))
        .unwrap();

    let got = index.knowledge_get("a.one").unwrap().expect("stored");
    assert_eq!(got.agent.as_deref(), Some("codex"));

    let codex = index.knowledge_by_agent("codex").unwrap();
    assert_eq!(codex.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), vec!["a.one"]);
    assert!(index.knowledge_by_agent("Codex").unwrap().is_empty());
    assert!(index.knowledge_by_agent("nobody").unwrap().is_empty());

    // JSON carries agent when present, omits it when absent.
    let json = serde_json::to_value(&got).unwrap();
    assert_eq!(json.get("agent").and_then(|v| v.as_str()), Some("codex"));
    let plain = index.knowledge_get("a.three").unwrap().expect("stored");
    assert!(serde_json::to_value(&plain).unwrap().get("agent").is_none());

    // Updates replace the writer identity wholesale, like evidence.
    let mut updated = got.clone();
    updated.agent = Some("cursor".to_string());
    index.knowledge_put(&updated).unwrap();
    assert_eq!(
        index.knowledge_get("a.one").unwrap().expect("stored").agent.as_deref(),
        Some("cursor")
    );
    assert!(index.knowledge_by_agent("codex").unwrap().is_empty());
}

#[test]
fn agent_validation_rejects_blank_and_oversize() {
    let (_dir, mut index, _hash) = indexed_project();
    let err = index
        .knowledge_put(&agent_record("bad.blank", Some("  "), KnowledgeOrigin::Agent))
        .unwrap_err();
    assert!(matches!(err, keel::KeelError::KnowledgeValidation(_)), "{err:?}");
    let err = index
        .knowledge_put(&agent_record("bad.long", Some(&"x".repeat(65)), KnowledgeOrigin::Agent))
        .unwrap_err();
    assert!(matches!(err, keel::KeelError::KnowledgeValidation(_)), "{err:?}");
}

#[test]
fn search_filters_compose_before_caps() {
    let (dir, mut index, _hash) = indexed_project();
    for (id, agent, origin) in [
        ("jwt.a", Some("codex"), KnowledgeOrigin::Agent),
        ("jwt.b", Some("codex"), KnowledgeOrigin::Agent),
        ("jwt.c", Some("claude-code"), KnowledgeOrigin::Agent),
        ("jwt.d", None, KnowledgeOrigin::Human),
    ] {
        index.knowledge_put(&agent_record(id, agent, origin)).unwrap();
    }
    let db = dir.path().join("index.db");
    drop(index);
    let conn = rusqlite::Connection::open(&db).unwrap();
    let codex = keel::knowledge::search_records_filtered(&conn, "jwt", None, Some("codex"), None)
        .unwrap();
    assert_eq!(codex.len(), 2);
    let human =
        keel::knowledge::search_records_filtered(&conn, "jwt", None, None, Some("HUMAN")).unwrap();
    assert_eq!(human.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), vec!["jwt.d"]);
    let both = keel::knowledge::search_records_filtered(
        &conn,
        "jwt",
        None,
        Some("codex"),
        Some("agent_generated"),
    )
    .unwrap();
    assert_eq!(both.len(), 2);
    let capped =
        keel::knowledge::search_records_filtered(&conn, "jwt", Some(1), Some("codex"), None)
            .unwrap();
    assert_eq!(capped.len(), 1);
}

#[test]
fn cli_status_filters_by_agent() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "pub fn alpha() {}\n").unwrap();
    assert!(run_knowledge(&["index", "."], root).0);

    let db = root.join(".keel/index.db");
    let mut index = Index::open(&db).unwrap();
    index
        .knowledge_put(&agent_record("k.one", Some("codex"), KnowledgeOrigin::Agent))
        .unwrap();
    index
        .knowledge_put(&agent_record("k.two", Some("claude-code"), KnowledgeOrigin::Agent))
        .unwrap();
    drop(index);

    let (ok, stdout) = run_knowledge(&["knowledge", "status", "--agent", "codex"], root);
    assert!(ok);
    assert!(stdout.contains("k.one"), "{stdout}");
    assert!(!stdout.contains("k.two"), "{stdout}");

    let (ok, stdout) = run_knowledge(&["knowledge", "status"], root);
    assert!(ok);
    assert!(stdout.contains("k.one"), "{stdout}");
    assert!(stdout.contains("k.two"), "{stdout}");
}
