//! Freshness engine: compare knowledge evidence pins against the live index.
//!
//! Deterministic invalidation is deliberately one-directional: positive
//! evidence of drift (removed file, renamed symbol, changed content) moves
//! `supported` → `stale`, while promotion back to `supported` only happens
//! through the explicit [`validate_record`] act — never automatically. A
//! record the engine cannot check (hashless evidence) is reported as
//! unverifiable, never downgraded on suspicion and never claimed fresh.

use crate::db::queries;
use crate::error::Result;
use crate::graph::target::resolve_file_target;
use crate::knowledge::model::{
    normalize_source_hash, now_timestamp, KnowledgeRecord, KnowledgeStatus,
};
use crate::knowledge::store::{get_record, list_records};
use rusqlite::Connection;
use serde::Serialize;
use std::collections::{HashMap, HashSet};

/// Live verdict for one record's evidence against the current index.
///
/// `fresh` means "nothing contradicts the claim", not "confirmed": entries
/// without hash pins cannot be checked and surface separately in
/// `unverifiable_evidence`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecordFreshness {
    /// Record id.
    pub id: String,
    /// Stored status (unchanged by the check itself).
    pub status: KnowledgeStatus,
    /// True when no evidence entry contradicts the claim.
    pub fresh: bool,
    /// Human-readable finding per non-fresh entry.
    pub reasons: Vec<String>,
    /// Entries whose pinned source verifiably drifted.
    pub stale_evidence: usize,
    /// Entries skipped for lack of a checkable pin.
    pub unverifiable_evidence: usize,
}

/// Report from [`invalidate_stale`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InvalidationReport {
    /// Supported records evaluated.
    pub checked: usize,
    /// Records transitioned to stale, with reasons.
    pub marked_stale: Vec<MarkedStale>,
}

/// One record transitioned to stale by [`invalidate_stale`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MarkedStale {
    /// Record id.
    pub id: String,
    /// Why its evidence no longer matches.
    pub reasons: Vec<String>,
}

/// Outcome of [`validate_record`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ValidationOutcome {
    /// All evidence resolves; pins refreshed to current revisions and the
    /// record promoted to `supported`. `repinned` counts entries whose pin
    /// was missing or differed.
    Validated {
        /// Record id.
        id: String,
        /// Evidence entries whose pin was written or refreshed.
        repinned: usize,
    },
    /// Evidence contradicts the claim (or is absent); the record is `stale`.
    Stale {
        /// Record id.
        id: String,
        /// Why verification failed.
        reasons: Vec<String>,
    },
    /// No record with this id exists; nothing was written.
    UnknownId {
        /// Requested id.
        id: String,
    },
}

/// One evidence entry's verdict against the index snapshot.
enum EvidenceCheck {
    Fresh,
    Changed(String),
    Unverifiable(String),
}

/// Cached index state for one freshness pass: file hashes plus per-file
/// symbol names, so N evidence entries cost O(files), not O(entries × scan).
struct IndexSnapshot<'c> {
    conn: &'c Connection,
    hashes: HashMap<String, String>,
    symbols: HashMap<String, HashSet<String>>,
}

impl<'c> IndexSnapshot<'c> {
    fn load(conn: &'c Connection) -> Result<Self> {
        Ok(Self {
            conn,
            hashes: queries::existing_hashes(conn)?,
            symbols: HashMap::new(),
        })
    }

    /// Resolve an evidence path to the indexed path: exact match first, then
    /// the shared suffix-tolerant resolver (bare basenames and subdir
    /// spellings agents commonly write).
    fn resolve(&self, raw: &str) -> Result<Option<String>> {
        if self.hashes.contains_key(raw) {
            return Ok(Some(raw.to_string()));
        }
        resolve_file_target(self.conn, raw)
    }

    /// Defined symbol names in one indexed file, cached per pass.
    fn symbols_in(&mut self, file: &str) -> Result<&HashSet<String>> {
        if !self.symbols.contains_key(file) {
            let names = queries::symbols_in_file(self.conn, file)?
                .into_iter()
                .map(|s| s.name)
                .collect();
            self.symbols.insert(file.to_string(), names);
        }
        // Just inserted above when missing.
        self.symbols
            .get(file)
            .ok_or_else(|| crate::error::KeelError::Mcp("freshness cache miss".to_string()))
    }
}

/// Anchor verdict: whether an evidence entry's file + symbol still exist.
enum Anchor {
    /// Anchors resolve; carries the indexed file path.
    Resolved(String),
    /// Anchors broken; carries the reason.
    Broken(String),
}

/// Check that an evidence entry's anchors exist (file indexed, symbol defined
/// when named). Hash comparison is deliberately not part of anchoring:
/// freshness checks compare hashes, while validation re-pins them.
fn check_anchors(
    snap: &mut IndexSnapshot,
    source_path: &str,
    symbol: Option<&str>,
) -> Result<Anchor> {
    let Some(indexed) = snap.resolve(source_path)? else {
        return Ok(Anchor::Broken(format!(
            "source `{source_path}` is not indexed (removed or never indexed)"
        )));
    };
    if let Some(symbol) = symbol {
        let defined = snap.symbols_in(&indexed)?;
        if !defined.contains(symbol) {
            return Ok(Anchor::Broken(missing_symbol_reason(
                snap.conn,
                &indexed,
                source_path,
                symbol,
            )?));
        }
    }
    Ok(Anchor::Resolved(indexed))
}

/// Check one evidence entry against the snapshot.
fn check_evidence(
    snap: &mut IndexSnapshot,
    source_path: &str,
    symbol: Option<&str>,
    source_hash: Option<&str>,
) -> Result<EvidenceCheck> {
    let indexed = match check_anchors(snap, source_path, symbol)? {
        Anchor::Broken(reason) => return Ok(EvidenceCheck::Changed(reason)),
        Anchor::Resolved(indexed) => indexed,
    };
    let Some(pin) = source_hash else {
        return Ok(EvidenceCheck::Unverifiable(format!(
            "`{source_path}` has no hash pin; freshness cannot be confirmed"
        )));
    };
    let normalized = match normalize_source_hash(pin) {
        Ok(h) => h,
        Err(_) => {
            return Ok(EvidenceCheck::Unverifiable(format!(
                "`{source_path}` has an invalid hash pin; freshness cannot be confirmed"
            )));
        }
    };
    // `indexed` came from the hash map or the resolver over indexed files.
    let current = snap.hashes.get(&indexed).map(String::as_str).unwrap_or("");
    if normalized != current {
        return Ok(EvidenceCheck::Changed(format!(
            "`{source_path}` changed since verification"
        )));
    }
    Ok(EvidenceCheck::Fresh)
}

/// Reason for a symbol missing from its evidence file. When the same name is
/// still defined elsewhere, hint at those files as a possible move — a hint,
/// not an identity claim (same-named symbols may be unrelated).
fn missing_symbol_reason(
    conn: &Connection,
    indexed: &str,
    source_path: &str,
    symbol: &str,
) -> Result<String> {
    let mut elsewhere: Vec<String> = queries::find_definition(conn, symbol)?
        .into_iter()
        .map(|s| s.file.to_string_lossy().into_owned())
        .filter(|f| f != indexed)
        .collect();
    elsewhere.sort();
    elsewhere.dedup();
    elsewhere.truncate(3);
    if elsewhere.is_empty() {
        Ok(format!(
            "symbol `{symbol}` no longer defined in `{source_path}` (renamed or removed)"
        ))
    } else {
        Ok(format!(
            "symbol `{symbol}` no longer defined in `{source_path}` (renamed, removed, or moved); still defined in {}",
            elsewhere.join(", ")
        ))
    }
}

/// Evaluate one record's evidence (pure read; no status is changed).
fn check_record(snap: &mut IndexSnapshot, record: &KnowledgeRecord) -> Result<RecordFreshness> {
    let mut reasons = Vec::new();
    let mut stale_evidence = 0usize;
    let mut unverifiable_evidence = 0usize;
    for e in &record.evidence {
        match check_evidence(
            snap,
            &e.source_path,
            e.symbol.as_deref(),
            e.source_hash.as_deref(),
        )? {
            EvidenceCheck::Fresh => {}
            EvidenceCheck::Changed(reason) => {
                stale_evidence += 1;
                reasons.push(reason);
            }
            EvidenceCheck::Unverifiable(reason) => {
                unverifiable_evidence += 1;
                reasons.push(reason);
            }
        }
    }
    Ok(RecordFreshness {
        id: record.id.clone(),
        status: record.status,
        fresh: stale_evidence == 0,
        reasons,
        stale_evidence,
        unverifiable_evidence,
    })
}

/// Check every record's evidence against the current index (read-only).
///
/// Records keep their stored status; the verdict is reported per record so
/// `knowledge status` can show stored-vs-live honestly.
pub fn check_freshness(conn: &Connection) -> Result<Vec<RecordFreshness>> {
    let mut snap = IndexSnapshot::load(conn)?;
    let mut out = Vec::new();
    for record in list_records(conn)? {
        out.push(check_record(&mut snap, &record)?);
    }
    Ok(out)
}

/// Check one record's evidence (read-only), or `None` when the id is unknown.
pub fn check_one(conn: &Connection, id: &str) -> Result<Option<RecordFreshness>> {
    let Some(record) = get_record(conn, id)? else {
        return Ok(None);
    };
    let mut snap = IndexSnapshot::load(conn)?;
    Ok(Some(check_record(&mut snap, &record)?))
}

/// Mark stale every `supported` record whose evidence no longer matches.
///
/// Only `supported` records transition: `unverified`/`contradicted` records
/// have no freshness claim to lose, and already-`stale` records are left
/// untouched (no timestamp churn). Unverifiable evidence never triggers a
/// transition on its own. Runs in one transaction.
pub fn invalidate_stale(conn: &mut Connection) -> Result<InvalidationReport> {
    let tx = conn.transaction()?;
    let mut snap = IndexSnapshot::load(&tx)?;
    let mut checked = 0usize;
    let mut marked_stale = Vec::new();
    for record in list_records(&tx)? {
        if record.status != KnowledgeStatus::Supported {
            continue;
        }
        checked += 1;
        let verdict = check_record(&mut snap, &record)?;
        if !verdict.fresh {
            tx.execute(
                "UPDATE knowledge SET status = 'stale', updated_at = ?1 WHERE id = ?2",
                rusqlite::params![now_timestamp(), verdict.id],
            )?;
            marked_stale.push(MarkedStale {
                id: verdict.id,
                reasons: verdict.reasons,
            });
        }
    }
    tx.commit()?;
    Ok(InvalidationReport {
        checked,
        marked_stale,
    })
}

/// Selectively revalidate one record: re-check its evidence, refresh pins to
/// current revisions, and promote to `supported` — or mark `stale`.
///
/// Validation checks anchor resolvability (evidence files indexed, named
/// symbols defined), not hash equality: absorbing content drift by re-pinning
/// is the point of revalidation, and a hash gate would make recovery from the
/// common "file edited" case impossible. Promotion stays the caller's
/// explicit act ("I checked, this is current") and the only path back from
/// `stale` — invalidation never auto-promotes. Records with no evidence, or
/// with unresolvable anchors, cannot validate and go `stale`. Provenance is
/// intentionally not consulted here: freshness transitions are mechanical and
/// must apply to human records too — that is the point of flagging facts for
/// revalidation when supporting code changes.
pub fn validate_record(conn: &mut Connection, id: &str) -> Result<ValidationOutcome> {
    let tx = conn.transaction()?;
    let Some(record) = get_record(&tx, id)? else {
        return Ok(ValidationOutcome::UnknownId { id: id.to_string() });
    };
    let mut snap = IndexSnapshot::load(&tx)?;
    if record.evidence.is_empty() {
        let outcome = mark_stale(&tx, &record, vec!["no evidence to verify against".to_string()])?;
        tx.commit()?;
        return Ok(outcome);
    }
    let mut broken = Vec::new();
    for e in &record.evidence {
        if let Anchor::Broken(reason) =
            check_anchors(&mut snap, &e.source_path, e.symbol.as_deref())?
        {
            broken.push(reason);
        }
    }
    if !broken.is_empty() {
        let outcome = mark_stale(&tx, &record, broken)?;
        tx.commit()?;
        return Ok(outcome);
    }
    // All evidence resolves: re-pin hashes to current revisions and promote.
    let mut repinned = 0usize;
    let mut stmt = tx.prepare(
        "UPDATE knowledge_evidence SET source_hash = ?1 WHERE knowledge_id = ?2 AND source_path = ?3 AND ((symbol IS NULL AND ?4 IS NULL) OR symbol IS ?4)",
    )?;
    for e in &record.evidence {
        // Checked above: every entry resolved to an indexed file.
        let indexed = snap
            .resolve(&e.source_path)?
            .unwrap_or_else(|| e.source_path.clone());
        let current = snap.hashes.get(&indexed).cloned().unwrap_or_default();
        let pinned = e
            .source_hash
            .as_deref()
            .and_then(|h| normalize_source_hash(h).ok());
        if pinned.as_deref() != Some(current.as_str()) {
            stmt.execute(rusqlite::params![
                current,
                record.id,
                e.source_path,
                e.symbol
            ])?;
            repinned += 1;
        }
    }
    drop(stmt);
    if record.status != KnowledgeStatus::Supported || repinned > 0 {
        tx.execute(
            "UPDATE knowledge SET status = 'supported', updated_at = ?1 WHERE id = ?2",
            rusqlite::params![now_timestamp(), record.id],
        )?;
    }
    tx.commit()?;
    Ok(ValidationOutcome::Validated {
        id: record.id,
        repinned,
    })
}

/// Mark `record` stale when it isn't already (no churn on repeat checks).
/// The caller owns the transaction commit.
fn mark_stale(
    tx: &rusqlite::Transaction,
    record: &KnowledgeRecord,
    reasons: Vec<String>,
) -> Result<ValidationOutcome> {
    if record.status != KnowledgeStatus::Stale {
        tx.execute(
            "UPDATE knowledge SET status = 'stale', updated_at = ?1 WHERE id = ?2",
            rusqlite::params![now_timestamp(), record.id],
        )?;
    }
    Ok(ValidationOutcome::Stale {
        id: record.id.clone(),
        reasons,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index;
    use crate::knowledge::model::{KnowledgeEvidence, KnowledgeOrigin, KnowledgeType};
    use crate::knowledge::store::put_record;
    use std::fs;
    use tempfile::tempdir;

    /// Two-file repo: `src/a.rs` defines `alpha`, `src/b.rs` defines `beta`.
    fn fixture() -> (tempfile::TempDir, Connection) {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.rs"), "pub fn alpha() {}\n").unwrap();
        fs::write(root.join("src/b.rs"), "pub fn beta() {}\n").unwrap();
        let mut conn = Connection::open_in_memory().unwrap();
        index::index_repository(root, &mut conn).unwrap();
        (dir, conn)
    }

    fn pin(conn: &Connection, path: &str) -> String {
        queries::existing_hashes(conn)
            .unwrap()
            .remove(path)
            .unwrap()
    }

    fn supported_record(
        id: &str,
        path: &str,
        symbol: Option<&str>,
        hash: Option<String>,
    ) -> KnowledgeRecord {
        KnowledgeRecord {
            id: id.to_string(),
            record_type: KnowledgeType::Fact,
            title: "T".to_string(),
            content: "C".to_string(),
            origin: KnowledgeOrigin::Agent,
            agent: None,
            status: KnowledgeStatus::Supported,
            created_at: String::new(),
            updated_at: String::new(),
            evidence: vec![KnowledgeEvidence {
                source_path: path.to_string(),
                symbol: symbol.map(str::to_string),
                source_hash: hash,
                commit_hash: None,
            }],
        }
    }

    fn freshness(conn: &Connection, id: &str) -> RecordFreshness {
        check_freshness(conn)
            .unwrap()
            .into_iter()
            .find(|f| f.id == id)
            .unwrap()
    }

    #[test]
    fn pinned_record_checks_fresh_and_survives_invalidation() {
        let (_dir, mut conn) = fixture();
        let hash = pin(&conn, "src/a.rs");
        put_record(
            &mut conn,
            &supported_record("k", "src/a.rs", Some("alpha"), Some(hash)),
        )
        .unwrap();

        let f = freshness(&conn, "k");
        assert!(f.fresh);
        assert_eq!(f.stale_evidence, 0);
        assert!(f.reasons.is_empty());

        let report = invalidate_stale(&mut conn).unwrap();
        assert_eq!(report.checked, 1);
        assert!(report.marked_stale.is_empty());
        assert_eq!(
            get_record(&conn, "k").unwrap().unwrap().status,
            KnowledgeStatus::Supported
        );
    }

    #[test]
    fn edited_file_marks_supported_stale() {
        let (dir, mut conn) = fixture();
        let hash = pin(&conn, "src/a.rs");
        put_record(
            &mut conn,
            &supported_record("k", "src/a.rs", Some("alpha"), Some(hash)),
        )
        .unwrap();
        let before = get_record(&conn, "k").unwrap().unwrap();

        fs::write(dir.path().join("src/a.rs"), "pub fn alpha() {}\n// note\n").unwrap();
        index::index_repository(dir.path(), &mut conn).unwrap();
        // The index pass itself auto-invalidates (wired below); assert the
        // end state rather than calling invalidate explicitly.
        let after = get_record(&conn, "k").unwrap().unwrap();
        assert_eq!(after.status, KnowledgeStatus::Stale);
        assert!(after.updated_at >= before.updated_at);
    }

    #[test]
    fn unrelated_change_keeps_supported() {
        let (dir, mut conn) = fixture();
        let hash = pin(&conn, "src/a.rs");
        put_record(
            &mut conn,
            &supported_record("k", "src/a.rs", Some("alpha"), Some(hash)),
        )
        .unwrap();

        // Touch only b.rs: the a.rs-pinned record must not flinch.
        fs::write(dir.path().join("src/b.rs"), "pub fn beta() {}\n// note\n").unwrap();
        index::index_repository(dir.path(), &mut conn).unwrap();

        assert_eq!(
            get_record(&conn, "k").unwrap().unwrap().status,
            KnowledgeStatus::Supported
        );
        assert!(freshness(&conn, "k").fresh);
    }

    #[test]
    fn deleted_file_marks_stale() {
        let (dir, mut conn) = fixture();
        let hash = pin(&conn, "src/a.rs");
        put_record(
            &mut conn,
            &supported_record("k", "src/a.rs", Some("alpha"), Some(hash)),
        )
        .unwrap();

        fs::remove_file(dir.path().join("src/a.rs")).unwrap();
        index::index_repository(dir.path(), &mut conn).unwrap();

        assert_eq!(
            get_record(&conn, "k").unwrap().unwrap().status,
            KnowledgeStatus::Stale
        );
        let report = invalidate_stale(&mut conn).unwrap();
        assert!(report.marked_stale.is_empty(), "no double marking");
    }

    #[test]
    fn renamed_symbol_marks_stale_with_reason() {
        let (dir, mut conn) = fixture();
        let hash = pin(&conn, "src/a.rs");
        put_record(
            &mut conn,
            &supported_record("k", "src/a.rs", Some("alpha"), Some(hash)),
        )
        .unwrap();

        fs::write(dir.path().join("src/a.rs"), "pub fn alpha_v2() {}\n").unwrap();
        index::index_repository(dir.path(), &mut conn).unwrap();

        assert_eq!(
            get_record(&conn, "k").unwrap().unwrap().status,
            KnowledgeStatus::Stale
        );
        // Fresh check names the missing symbol (rename, not just "changed").
        let f = freshness(&conn, "k");
        assert!(!f.fresh);
        assert!(
            f.reasons.iter().any(|r| r.contains("alpha") && r.contains("renamed")),
            "reasons: {:?}",
            f.reasons
        );
    }

    #[test]
    fn moved_symbol_hints_at_new_location() {
        let (dir, mut conn) = fixture();
        let hash = pin(&conn, "src/a.rs");
        put_record(
            &mut conn,
            &supported_record("k", "src/a.rs", Some("alpha"), Some(hash)),
        )
        .unwrap();

        // Move `alpha` from a.rs to b.rs.
        fs::write(dir.path().join("src/a.rs"), "// empty now\n").unwrap();
        fs::write(
            dir.path().join("src/b.rs"),
            "pub fn beta() {}\npub fn alpha() {}\n",
        )
        .unwrap();
        index::index_repository(dir.path(), &mut conn).unwrap();

        let f = freshness(&conn, "k");
        assert!(!f.fresh);
        assert!(
            f.reasons
                .iter()
                .any(|r| r.contains("alpha") && r.contains("src/b.rs")),
            "reasons: {:?}",
            f.reasons
        );
    }

    #[test]
    fn non_supported_records_are_checked_but_never_transitioned() {
        let (dir, mut conn) = fixture();
        let hash = pin(&conn, "src/a.rs");
        let mut unverified = supported_record("u", "src/a.rs", Some("alpha"), Some(hash.clone()));
        unverified.status = KnowledgeStatus::Unverified;
        put_record(&mut conn, &unverified).unwrap();
        let mut stale = supported_record("s", "src/a.rs", Some("alpha"), Some(hash));
        stale.status = KnowledgeStatus::Stale;
        put_record(&mut conn, &stale).unwrap();
        let stale_before = get_record(&conn, "s").unwrap().unwrap();

        fs::write(dir.path().join("src/a.rs"), "pub fn alpha() {}\n// x\n").unwrap();
        index::index_repository(dir.path(), &mut conn).unwrap();

        // The check sees the drift...
        assert!(!freshness(&conn, "u").fresh);
        assert!(!freshness(&conn, "s").fresh);
        // ...but neither record transitions or churns.
        assert_eq!(
            get_record(&conn, "u").unwrap().unwrap().status,
            KnowledgeStatus::Unverified
        );
        let stale_after = get_record(&conn, "s").unwrap().unwrap();
        assert_eq!(stale_after.status, KnowledgeStatus::Stale);
        assert_eq!(stale_after.updated_at, stale_before.updated_at);
        let report = invalidate_stale(&mut conn).unwrap();
        assert_eq!(report.checked, 0);
    }

    #[test]
    fn hashless_evidence_is_unverifiable_never_stale() {
        let (dir, mut conn) = fixture();
        put_record(
            &mut conn,
            &supported_record("k", "src/a.rs", Some("alpha"), None),
        )
        .unwrap();

        fs::write(dir.path().join("src/a.rs"), "pub fn alpha() {}\n// x\n").unwrap();
        index::index_repository(dir.path(), &mut conn).unwrap();

        // Suspicion is not evidence: the record keeps its status.
        assert_eq!(
            get_record(&conn, "k").unwrap().unwrap().status,
            KnowledgeStatus::Supported
        );
        let f = freshness(&conn, "k");
        assert!(f.fresh);
        assert_eq!(f.unverifiable_evidence, 1);
    }

    #[test]
    fn validate_repins_and_promotes_after_drift() {
        let (dir, mut conn) = fixture();
        let hash = pin(&conn, "src/a.rs");
        put_record(
            &mut conn,
            &supported_record("k", "src/a.rs", Some("alpha"), Some(hash)),
        )
        .unwrap();

        fs::write(dir.path().join("src/a.rs"), "pub fn alpha() {}\n// x\n").unwrap();
        index::index_repository(dir.path(), &mut conn).unwrap();
        assert_eq!(
            get_record(&conn, "k").unwrap().unwrap().status,
            KnowledgeStatus::Stale
        );

        match validate_record(&mut conn, "k").unwrap() {
            ValidationOutcome::Validated { id, repinned } => {
                assert_eq!(id, "k");
                assert_eq!(repinned, 1);
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
        let record = get_record(&conn, "k").unwrap().unwrap();
        assert_eq!(record.status, KnowledgeStatus::Supported);
        assert_eq!(record.evidence[0].source_hash.as_deref(), Some(pin(&conn, "src/a.rs").as_str()));
        // A second validation is a no-op write (no churn).
        match validate_record(&mut conn, "k").unwrap() {
            ValidationOutcome::Validated { repinned, .. } => assert_eq!(repinned, 0),
            other => panic!("unexpected outcome: {other:?}"),
        }
    }

    #[test]
    fn validate_pins_hashless_evidence() {
        let (_dir, mut conn) = fixture();
        put_record(
            &mut conn,
            &supported_record("k", "src/a.rs", Some("alpha"), None),
        )
        .unwrap();
        match validate_record(&mut conn, "k").unwrap() {
            ValidationOutcome::Validated { repinned, .. } => assert_eq!(repinned, 1),
            other => panic!("unexpected outcome: {other:?}"),
        }
        assert!(freshness(&conn, "k").unverifiable_evidence == 0);
    }

    #[test]
    fn validate_unknown_id_and_evidence_less_records() {
        let (_dir, mut conn) = fixture();
        assert!(matches!(
            validate_record(&mut conn, "nope").unwrap(),
            ValidationOutcome::UnknownId { .. }
        ));

        let mut bare = supported_record("bare", "src/a.rs", Some("alpha"), None);
        bare.evidence.clear();
        put_record(&mut conn, &bare).unwrap();
        match validate_record(&mut conn, "bare").unwrap() {
            ValidationOutcome::Stale { reasons, .. } => {
                assert!(reasons.iter().any(|r| r.contains("no evidence")), "{reasons:?}");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
        assert_eq!(
            get_record(&conn, "bare").unwrap().unwrap().status,
            KnowledgeStatus::Stale
        );
    }

    #[test]
    fn validate_keeps_stale_when_symbol_gone() {
        let (dir, mut conn) = fixture();
        let hash = pin(&conn, "src/a.rs");
        put_record(
            &mut conn,
            &supported_record("k", "src/a.rs", Some("alpha"), Some(hash)),
        )
        .unwrap();

        fs::write(dir.path().join("src/a.rs"), "pub fn other() {}\n").unwrap();
        index::index_repository(dir.path(), &mut conn).unwrap();

        match validate_record(&mut conn, "k").unwrap() {
            ValidationOutcome::Stale { reasons, .. } => {
                assert!(reasons.iter().any(|r| r.contains("alpha")), "{reasons:?}");
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }
}
