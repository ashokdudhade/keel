//! End-to-end ADR tests: facade list/show and the `keel adr` CLI.

use keel::{
    AdrDeclared, AdrVerdict, Index, KnowledgeEvidence, KnowledgeOrigin, KnowledgeRecord,
    KnowledgeType,
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
    fs::create_dir_all(root.join("docs/adr")).unwrap();
    fs::create_dir_all(root.join("notes")).unwrap();
    fs::write(root.join("src/auth.rs"), "pub fn login() {}\n").unwrap();
    fs::write(
        root.join("docs/adr/001-sessions.md"),
        "# Use session tokens\n\
         \n\
         Status: accepted\n\
         Date: 2026-02-01\n\
         \n\
         `login` handles auth; see `src/auth.rs`. Dropped `zzz_old`.\n\
         The `use` keyword needs no link.\n",
    )
    .unwrap();
    fs::write(root.join("docs/adr/002-cache.md"), "Thinking out loud, no claims.\n").unwrap();
    fs::write(root.join("notes/random.md"), "Mentions `zzz_old`.\n").unwrap();
    dir
}

fn indexed(root: &std::path::Path) -> Index {
    let mut index = Index::open_in_memory().unwrap();
    index.index_path(root).unwrap();
    index
}

fn put_decision(index: &mut Index) {
    let mut record = KnowledgeRecord::new(
        "auth.sessions",
        KnowledgeType::Decision,
        "Session tokens",
        "Sessions use login from src/auth.rs",
        KnowledgeOrigin::Human,
    );
    record.evidence.push(KnowledgeEvidence {
        source_path: "src/auth.rs".to_string(),
        symbol: Some("login".to_string()),
        source_hash: None,
        commit_hash: None,
    });
    index.knowledge_put(&record).unwrap();
}

#[test]
fn list_reports_verdicts_and_declared_status() {
    let dir = fixture_project();
    let index = indexed(dir.path());
    let list = index.adr_list(dir.path()).unwrap();
    assert_eq!(list.adrs.len(), 2);

    let first = &list.adrs[0];
    assert_eq!(first.id, "001-sessions");
    assert_eq!(first.title.as_deref(), Some("Use session tokens"));
    assert_eq!(first.declared, AdrDeclared::Accepted);
    assert_eq!(first.path, "docs/adr/001-sessions.md");
    // login (live) + src/auth.rs (live) + zzz_old (dead); `use` skips.
    assert_eq!(first.links, 3);
    assert_eq!(first.verdict, AdrVerdict::Stale);

    let second = &list.adrs[1];
    assert_eq!(second.id, "002-cache");
    assert!(second.title.is_none());
    assert_eq!(second.declared, AdrDeclared::Missing);
    assert_eq!(second.links, 0);
    assert_eq!(second.verdict, AdrVerdict::Unlinked);
}

#[test]
fn show_links_implementation_and_knowledge() {
    let dir = fixture_project();
    let mut index = indexed(dir.path());
    put_decision(&mut index);
    let detail = index
        .adr_show(dir.path(), "001-sessions")
        .unwrap()
        .expect("ADR found");
    assert_eq!(detail.declared, AdrDeclared::Accepted);
    assert_eq!(detail.date.as_deref(), Some("2026-02-01"));
    assert_eq!(detail.verdict, AdrVerdict::Stale);

    let claims: Vec<(&str, bool)> = detail
        .links
        .iter()
        .map(|l| (l.claim.as_str(), l.live))
        .collect();
    assert_eq!(
        claims,
        vec![("login", true), ("src/auth.rs", true), ("zzz_old", false)]
    );
    let login = &detail.links[0];
    assert_eq!(login.kind, "symbol");
    assert_eq!(login.line, 6);
    assert_eq!(login.target.as_deref(), Some("src/auth.rs:1"));
    let file = &detail.links[1];
    assert_eq!(file.kind, "file");
    assert_eq!(file.target.as_deref(), Some("src/auth.rs"));
    assert!(detail.links[2].target.is_none());

    assert_eq!(detail.records.len(), 1);
    assert_eq!(detail.records[0].id, "auth.sessions");

    // Plain tempdir: history degrades to an honest note, never fails.
    assert!(detail.history.is_empty());
    assert!(detail.notes.iter().any(|n| n.contains("not a git checkout")));
}

#[test]
fn show_accepts_paths_and_misses_honestly() {
    let dir = fixture_project();
    let index = indexed(dir.path());
    let by_rel = index
        .adr_show(dir.path(), "docs/adr/001-sessions.md")
        .unwrap();
    assert!(by_rel.is_some());
    let abs = dir.path().join("docs/adr/002-cache.md");
    let by_abs = index
        .adr_show(dir.path(), abs.to_str().unwrap())
        .unwrap();
    assert_eq!(by_abs.map(|d| d.id), Some("002-cache".to_string()));
    assert!(index.adr_show(dir.path(), "nope").unwrap().is_none());

    let suggestions = keel::adr::suggest_adr_ids(dir.path(), "session");
    assert_eq!(suggestions, vec!["001-sessions".to_string()]);
    assert!(keel::adr::suggest_adr_ids(dir.path(), "zzz").is_empty());
}

#[test]
fn sweeps_are_deterministic() {
    let dir = fixture_project();
    let index = indexed(dir.path());
    assert_eq!(
        index.adr_list(dir.path()).unwrap(),
        index.adr_list(dir.path()).unwrap()
    );
    assert_eq!(
        index.adr_show(dir.path(), "001-sessions").unwrap(),
        index.adr_show(dir.path(), "001-sessions").unwrap()
    );
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn history_reads_git_log_for_linked_files() {
    if !git_available() {
        eprintln!("skipping: git binary not available");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("docs/adr")).unwrap();
    fs::write(root.join("src/auth.rs"), "pub fn login() {}\n").unwrap();
    fs::write(
        root.join("docs/adr/001-sessions.md"),
        "# Use session tokens\n\nStatus: accepted\n\n`login` handles auth.\n",
    )
    .unwrap();
    let run = |args: &[&str]| {
        let status = Command::new("git")
            .args(args)
            .current_dir(root)
            .status()
            .unwrap();
        assert!(status.success(), "{args:?}");
    };
    run(&["init", "-q"]);
    run(&["add", "."]);
    run(&[
        "-c", "user.email=test@example.com", "-c", "user.name=Test", "commit", "-q", "-m",
        "decide sessions",
    ]);
    let index = indexed(root);
    let detail = index
        .adr_show(root, "001-sessions")
        .unwrap()
        .expect("ADR found");
    assert_eq!(detail.history.len(), 1);
    assert_eq!(detail.history[0].subject, "decide sessions");
    assert_eq!(detail.history[0].hash.len(), 40);
    assert_eq!(detail.history[0].date.len(), 10);
}

#[test]
fn cli_list_show_and_miss() {
    let dir = fixture_project();
    let list = Command::new(keel_bin())
        .args(["adr", "list"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(list.status.success());
    let out = String::from_utf8(list.stdout).unwrap();
    assert!(out.contains("001-sessions"), "{out}");
    assert!(out.contains("stale"), "{out}");
    assert!(out.contains("002-cache"), "{out}");
    assert!(out.contains("unlinked"), "{out}");

    let show = Command::new(keel_bin())
        .args(["adr", "show", "001-sessions"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(show.status.success());
    let show_out = String::from_utf8(show.stdout).unwrap();
    assert!(show_out.contains("Use session tokens"), "{show_out}");
    assert!(show_out.contains("[symbol live]"), "{show_out}");
    assert!(show_out.contains("[symbol dead]"), "{show_out}");
    assert!(show_out.contains("knowledge:"), "{show_out}");
    assert!(show_out.contains("history:"), "{show_out}");

    let miss = Command::new(keel_bin())
        .args(["adr", "show", "sessions"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(!miss.status.success());
    let miss_err = String::from_utf8(miss.stderr).unwrap();
    assert!(miss_err.contains("did you mean"), "{miss_err}");
    assert!(miss_err.contains("001-sessions"), "{miss_err}");
}

#[test]
fn cli_json_shapes() {
    let dir = fixture_project();
    let list = Command::new(keel_bin())
        .args(["adr", "list", "--json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(list.status.success());
    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8(list.stdout).unwrap()).unwrap();
    assert_eq!(parsed.get("adrs").and_then(|v| v.as_array()).unwrap().len(), 2);

    let show = Command::new(keel_bin())
        .args(["adr", "show", "001-sessions", "--json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert!(show.status.success());
    let parsed: serde_json::Value =
        serde_json::from_str(&String::from_utf8(show.stdout).unwrap()).unwrap();
    assert_eq!(parsed.get("verdict").and_then(|v| v.as_str()), Some("stale"));
    assert_eq!(
        parsed.get("links").and_then(|v| v.as_array()).unwrap().len(),
        3
    );
}
