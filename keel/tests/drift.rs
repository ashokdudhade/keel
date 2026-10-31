//! End-to-end drift tests: facade sweeps and the `keel drift` CI gate.

use keel::{DriftVerdict, Index};
use std::fs;
use std::process::Command;

fn keel_bin() -> &'static str {
    env!("CARGO_BIN_EXE_keel")
}

/// Fixture project: a small Rust lib plus docs exercising every verdict.
///
/// `docs/arch.md` line map (1-based):
/// - 3: `new_thing` (ok) — 4: `new_things` (stale, suggests `new_thing`)
/// - 5: `zzz_nope` (unsupported) — 6: struct `helper` (contradicted: fn)
/// - 7: `Widget.show` (ok member) — 8: `Gadget.show` (unsupported member)
/// - 9: `src/lib.rs` (ok path) — 10: `src/gone.rs` (unsupported path)
/// - 11: trait `Widget` (contradicted: struct) — 12: struct `dup`
///   (ambiguous: defined twice, kind check skips) — 14: fenced span skipped.
fn fixture_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::create_dir_all(root.join("docs/keel")).unwrap();
    fs::create_dir_all(root.join("notes")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub fn new_thing() {}\n\
         pub struct Widget;\n\
         impl Widget {\n    pub fn show(&self) {}\n}\n\
         pub fn helper() {}\n",
    )
    .unwrap();
    fs::write(root.join("src/a.rs"), "pub fn dup() {}\n").unwrap();
    fs::write(root.join("src/b.rs"), "pub fn dup() {}\n").unwrap();
    fs::write(
        root.join("docs/arch.md"),
        "# Architecture\n\
         \n\
         Entry point is `new_thing`.\n\
         The plural `new_things` was the old name.\n\
         Nothing uses `zzz_nope` anymore.\n\
         The struct `helper` holds shared state.\n\
         Renders via `Widget.show`.\n\
         Legacy `Gadget.show` path retired.\n\
         See `src/lib.rs` for details.\n\
         Old diagram at `src/gone.rs`.\n\
         The trait `Widget` abstracts storage.\n\
         The struct `dup` is defined twice, ambiguity skips kinds.\n\
         ```rust\n\
         let `hidden_claim` = 1;\n\
         ```\n",
    )
    .unwrap();
    fs::write(root.join("AGENTS.md"), "Run `new_thing` first.\n").unwrap();
    fs::write(root.join("notes/adr-001.md"), "Dropped `zzz_nope`.\n").unwrap();
    // Out of scope: must never produce findings.
    fs::write(root.join("notes/random.md"), "Mentions `zzz_nope`.\n").unwrap();
    fs::write(root.join("README.md"), "Mentions `zzz_nope`.\n").unwrap();
    fs::write(root.join("docs/keel/old.md"), "Mentions `zzz_nope`.\n").unwrap();
    dir
}

fn indexed(root: &std::path::Path) -> Index {
    let mut index = Index::open_in_memory().unwrap();
    index.index_path(root).unwrap();
    index
}

fn finding<'a>(
    report: &'a keel::DriftReport,
    doc: &str,
    line: u32,
) -> Option<&'a keel::DriftFinding> {
    report
        .findings
        .iter()
        .find(|f| f.doc == doc && f.line == line)
}

#[test]
fn three_verdicts_with_lines_and_suggestions() {
    let dir = fixture_project();
    let index = indexed(dir.path());
    let report = index.drift_check(dir.path(), None).unwrap();

    // Stale: gone, but a rename candidate exists.
    let stale = finding(&report, "docs/arch.md", 4).expect("stale finding");
    assert_eq!(stale.claim, "new_things");
    assert_eq!(stale.verdict, DriftVerdict::PotentiallyStale);
    assert!(stale.evidence.contains("no symbol named `new_things`"));
    let suggestion = stale.suggestion.as_ref().expect("rename suggestion");
    assert!(suggestion.contains("`new_thing`"), "{suggestion}");
    assert!(suggestion.contains("possible rename"), "{suggestion}");

    // Unsupported: nothing resembles it.
    let unsupported = finding(&report, "docs/arch.md", 5).expect("unsupported finding");
    assert_eq!(unsupported.claim, "zzz_nope");
    assert_eq!(unsupported.verdict, DriftVerdict::Unsupported);
    assert!(unsupported.suggestion.is_none());

    // Contradicted: resolves, but the kind disagrees.
    let contra_fn = finding(&report, "docs/arch.md", 6).expect("contradicted fn");
    assert_eq!(contra_fn.verdict, DriftVerdict::Contradicted);
    assert!(contra_fn.evidence.contains("a struct"), "{}", contra_fn.evidence);
    assert!(contra_fn.evidence.contains("a function"), "{}", contra_fn.evidence);
    assert_eq!(contra_fn.suggestion.as_deref(), Some("defined as function"));

    let contra_struct = finding(&report, "docs/arch.md", 11).expect("contradicted struct");
    assert_eq!(contra_struct.verdict, DriftVerdict::Contradicted);
    assert!(contra_struct.evidence.contains("a trait"), "{}", contra_struct.evidence);
    assert!(contra_struct.evidence.contains("a struct"), "{}", contra_struct.evidence);

    // Member on the wrong container: unsupported with holders named.
    let member = finding(&report, "docs/arch.md", 8).expect("member finding");
    assert_eq!(member.verdict, DriftVerdict::Unsupported);
    assert!(member.evidence.contains("no member `show` on `Gadget`"));
    let member_suggestion = member.suggestion.as_ref().expect("holder suggestion");
    assert!(member_suggestion.contains("Widget"), "{member_suggestion}");

    // Broken link: unsupported path claim.
    let path = finding(&report, "docs/arch.md", 10).expect("path finding");
    assert_eq!(path.claim, "src/gone.rs");
    assert_eq!(path.verdict, DriftVerdict::Unsupported);

    // Clean claims stay quiet: ok symbol (3), ok member (7), ok path (9),
    // ambiguous kind (12), fenced span (14).
    for quiet in [3u32, 7, 9, 12, 14] {
        assert!(finding(&report, "docs/arch.md", quiet).is_none(), "line {quiet}");
    }

    // ADR in scope, random notes / README / generated wiki out of scope.
    assert!(finding(&report, "notes/adr-001.md", 1).is_some());
    for f in &report.findings {
        assert!(
            !f.doc.starts_with("docs/keel/") && f.doc != "README.md" && f.doc != "notes/random.md",
            "out-of-scope finding: {}",
            f.doc
        );
    }
    // The wikiless `docs/keel` dir is noted, not silently ignored.
    assert!(report.notes.iter().any(|n| n.contains("without a wiki manifest")));
}

#[test]
fn discovery_scope_is_exact() {
    let dir = fixture_project();
    let docs = keel::drift::discover_docs(dir.path());
    let rels: Vec<String> = docs
        .iter()
        .map(|p| {
            p.strip_prefix(dir.path()).unwrap().to_string_lossy().replace('\\', "/")
        })
        .collect();
    assert_eq!(rels, vec!["AGENTS.md", "docs/arch.md", "notes/adr-001.md"]);
}

#[test]
fn sweeps_are_deterministic() {
    let dir = fixture_project();
    let index = indexed(dir.path());
    let first = index.drift_check(dir.path(), None).unwrap();
    let second = index.drift_check(dir.path(), None).unwrap();
    assert_eq!(first, second);
}

#[test]
fn empty_index_skips_symbols_but_checks_paths() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("docs")).unwrap();
    fs::write(
        dir.path().join("docs/a.md"),
        "Calls `zzz_nope` per `missing/x.rs`.\n",
    )
    .unwrap();
    let index = Index::open_in_memory().unwrap();
    let report = index.drift_check(dir.path(), None).unwrap();
    assert!(report.notes.iter().any(|n| n.contains("no symbols")));
    assert_eq!(report.findings.len(), 1);
    assert_eq!(report.findings[0].claim, "missing/x.rs");
}

#[test]
fn stale_wiki_pages_become_page_level_findings() {
    let dir = fixture_project();
    let mut index = indexed(dir.path());
    let out = dir.path().join("docs").join("keel");
    index.wiki_generate(&out).unwrap();
    // Change indexed content, reindex, and sweep: pins must mismatch.
    fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn new_thing() {}\npub fn brand_new() {}\n",
    )
    .unwrap();
    index.index_path(dir.path()).unwrap();
    let report = index.drift_check(dir.path(), None).unwrap();
    assert!(report.wiki_pages_checked > 0);
    let wiki_findings: Vec<_> = report
        .findings
        .iter()
        .filter(|f| f.doc.starts_with("docs/keel/"))
        .collect();
    assert!(!wiki_findings.is_empty());
    for wiki in wiki_findings {
        assert_eq!(wiki.line, 0);
        assert_eq!(wiki.verdict, DriftVerdict::PotentiallyStale);
        assert!(wiki.suggestion.as_deref().unwrap_or("").contains("wiki update"));
    }
}

#[test]
fn missing_scoped_paths_are_noted() {
    let dir = fixture_project();
    let index = indexed(dir.path());
    let report = index
        .drift_check(dir.path(), Some(&[std::path::PathBuf::from("docs/nope.md")]))
        .unwrap();
    assert_eq!(report.docs_checked, 1);
    assert!(report.findings.is_empty());
    assert!(report.notes.iter().any(|n| n.contains("scoped path not found")));
}

#[test]
fn discovery_cap_reports_instead_of_silently_cutting() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("docs")).unwrap();
    for n in 0..505 {
        fs::write(dir.path().join(format!("docs/p{n:03}.md")), "# t\n").unwrap();
    }
    let index = indexed(dir.path());
    let report = index.drift_check(dir.path(), None).unwrap();
    assert_eq!(report.docs_checked, 500);
    assert!(report.notes.iter().any(|n| n.contains("capped at 500")));
}

#[test]
fn per_doc_finding_cap_reports_overflow() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("docs")).unwrap();
    // A non-empty index: otherwise symbol checks skip and nothing floods.
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/lib.rs"), "pub fn keep() {}\n").unwrap();
    let mut text = String::from("# t\n");
    for n in 0..105 {
        text.push_str(&format!("Broken `zzz_nope_{n}` here.\n"));
    }
    fs::write(dir.path().join("docs/flood.md"), text).unwrap();
    let index = indexed(dir.path());
    let report = index.drift_check(dir.path(), None).unwrap();
    assert_eq!(report.findings.len(), 100);
    assert!(report.notes.iter().any(|n| n.contains("capped at 100")));
}

#[test]
fn path_claims_resolve_by_basename_suffix_and_dirs() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src/deep")).unwrap();
    fs::create_dir_all(root.join("other")).unwrap();
    fs::create_dir_all(root.join("assets")).unwrap();
    fs::create_dir_all(root.join("docs/sub")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn keep() {}\n").unwrap();
    fs::write(root.join("src/deep/nested.rs"), "pub fn deep() {}\n").unwrap();
    fs::write(root.join("src/dup.rs"), "pub fn a() {}\n").unwrap();
    fs::write(root.join("other/dup.rs"), "pub fn b() {}\n").unwrap();
    fs::write(
        root.join("docs/sub/guide.md"),
        "# Guide\n\
         Unique basename `nested.rs` resolves.\n\
         Unique suffix `deep/nested.rs` resolves.\n\
         Unique dir `assets/` resolves.\n\
         Ambiguous `dup.rs` skips quietly.\n\
         Missing `gone/deep.rs` flags.\n\
         Extension `.rs` is not a claim.\n",
    )
    .unwrap();
    let index = indexed(root);
    let report = index.drift_check(root, None).unwrap();
    assert_eq!(report.findings.len(), 1, "{:?}", report.findings);
    assert_eq!(report.findings[0].claim, "gone/deep.rs");
    assert_eq!(report.findings[0].verdict, keel::DriftVerdict::Unsupported);
    assert!(report.notes.iter().any(|n| n.contains("ambiguous path claim")));
}

#[test]
fn miss_ladder_stays_quiet_on_known_names() {
    let dir = tempfile::tempdir().unwrap();
    // Fixed project name so the root-basename rung is pinnable.
    let root = dir.path().join("myproj");
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("docs")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "use serde::Serialize as Ser;\n\
         pub fn keep() {}\n\
         pub struct High;\n\
         pub fn bound<T: Sync>() {}\n\
         pub fn get() {}\n\
         pub fn run(db: u32) -> u32 { return db; }\n",
    )
    .unwrap();
    fs::write(
        root.join("docs/calm.md"),
        "# Calm\n\
         Keyword `use`, tool `cargo`, shorthand `ts`.\n\
         External `serde`, alias `Ser`.\n\
         Schema `symbols` plus `symbols.container`.\n\
         Case `high` matches `High`.\n\
         Env `KEEL_INDEX_DB` quiet but `MAX_RESULT_LIMIT` flags.\n\
         Project `myproj`; stranger `otherproj` flags.\n\
         MCP `definition`, verbs `drift` and `status`.\n\
         Receiver `self.save` skips; `Type.member` flags.\n\
         `GET /health` `tools/call` `.ts/.tsx` `v1.4.1` `src/lib.rs:2` `C` `b::f()` quiet.\n\
         `watch.log` `projects.json` `.keel/index.db` `insights.port` `usage.jsonl` runtime-quiet.\n\
         Hybrid `src/lib.rs::keep` resolves; `src/lib.rs::bogus` flags.\n\
         Borrowed `Sync` and value receiver `db.get` stay quiet.\n\
         `text/html` `A=/b/c` `fetch('x')` `a\\tb` quiet; `model/user.py` flags.\n\
         Meta `keel_version` resolves.\n",
    )
    .unwrap();
    let index = indexed(&root);
    let report = index.drift_check(&root, None).unwrap();
    let claims: Vec<(&str, u32)> = report
        .findings
        .iter()
        .map(|f| (f.claim.as_str(), f.line))
        .collect();
    assert_eq!(
        claims,
        vec![
            ("MAX_RESULT_LIMIT", 6),
            ("otherproj", 7),
            ("Type.member", 9),
            ("src/lib.rs::bogus", 12),
            ("model/user.py", 14),
        ]
    );
    for finding in &report.findings {
        assert_eq!(finding.verdict, keel::DriftVerdict::Unsupported);
    }
    // Both-missing member form flags without noise suggestions.
    let member = finding(&report, "docs/calm.md", 9).unwrap();
    assert!(member.suggestion.is_none());
    assert!(member.evidence.contains("neither `Type` nor `member`"));
}

#[test]
fn cli_gate_fails_on_findings_and_passes_when_clean() {
    let dir = fixture_project();
    // Dirty project: exit 1 with rows, suggestions, and a summary.
    let dirty = Command::new(keel_bin())
        .arg("drift")
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(dirty.status.code(), Some(1));
    let out = String::from_utf8(dirty.stdout).unwrap();
    assert!(out.contains("[potentially stale]"), "{out}");
    assert!(out.contains("[contradicted]"), "{out}");
    assert!(out.contains("suggestion:"), "{out}");
    assert!(out.contains("finding(s) across"), "{out}");

    // Scoped to the clean agent file: exit 0.
    let scoped = Command::new(keel_bin())
        .args(["drift", "--path", "AGENTS.md"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(scoped.status.code(), Some(0));
    let scoped_out = String::from_utf8(scoped.stdout).unwrap();
    assert!(scoped_out.contains("No drift detected"), "{scoped_out}");

    // Missing scoped path: noted, still exit 0 without findings.
    let missing = Command::new(keel_bin())
        .args(["drift", "--path", "docs/nope.md"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(0));
    let missing_out = String::from_utf8(missing.stdout).unwrap();
    assert!(missing_out.contains("scoped path not found"), "{missing_out}");
}

#[test]
fn cli_json_stays_machine_readable() {
    let dir = fixture_project();
    let output = Command::new(keel_bin())
        .args(["drift", "--json"])
        .current_dir(dir.path())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    let stdout = String::from_utf8(output.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(parsed.get("docs_checked").and_then(|v| v.as_u64()).unwrap() >= 3);
    let findings = parsed.get("findings").and_then(|v| v.as_array()).unwrap();
    assert!(!findings.is_empty());
    assert!(findings.iter().any(|f| f.get("verdict").and_then(|v| v.as_str()) == Some("potentially_stale")));
    assert!(findings.iter().any(|f| f.get("verdict").and_then(|v| v.as_str()) == Some("contradicted")));
}
