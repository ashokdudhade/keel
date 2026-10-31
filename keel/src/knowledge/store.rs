//! Knowledge persistence: provenance-checked CRUD over SQLite.
//!
//! Writes go through [`put_record`], which validates, stamps timestamps, and
//! enforces the provenance rule (human-authored records reject non-human
//! overwrites). Reads ([`get_record`], [`list_records`]) parse leniently so
//! newer writers never brick older readers.

use crate::error::{KeelError, Result};
use crate::knowledge::model::{
    normalize_source_hash, now_timestamp, KnowledgeEvidence, KnowledgeOrigin, KnowledgeRecord,
    KnowledgeStatus, KnowledgeType,
};
use rusqlite::{params, Connection, OptionalExtension};

/// Insert or update a knowledge record with its evidence set.
///
/// Validation gates every write. Timestamps are store-owned: inserts stamp
/// both to now, updates preserve `created_at` and stamp `updated_at` —
/// caller-supplied values are ignored, never trusted. Evidence is replaced
/// wholesale per write. When `id` already exists with a human origin, only a
/// human-origin write is accepted ([`KeelError::KnowledgeProvenance`]).
pub fn put_record(conn: &mut Connection, record: &KnowledgeRecord) -> Result<()> {
    record.validate()?;
    let id = record.id.clone();
    let now = now_timestamp();
    let tx = conn.transaction()?;

    let existing_origin: Option<String> = tx
        .query_row(
            "SELECT origin FROM knowledge WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(prev) = existing_origin.as_deref() {
        if KnowledgeOrigin::from_db(prev).is_human() && !record.origin.is_human() {
            return Err(KeelError::KnowledgeProvenance {
                id,
                origin: record.origin.as_db(),
            });
        }
        tx.execute(
            "UPDATE knowledge SET type = ?1, title = ?2, content = ?3, origin = ?4, status = ?5, updated_at = ?6 WHERE id = ?7",
            params![
                record.record_type.as_db(),
                record.title,
                record.content,
                record.origin.as_db(),
                record.status.as_db(),
                now,
                id,
            ],
        )?;
    } else {
        tx.execute(
            "INSERT INTO knowledge (id, type, title, content, origin, status, created_at, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                record.record_type.as_db(),
                record.title,
                record.content,
                record.origin.as_db(),
                record.status.as_db(),
                now,
                now,
            ],
        )?;
    }

    tx.execute(
        "DELETE FROM knowledge_evidence WHERE knowledge_id = ?1",
        params![id],
    )?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO knowledge_evidence (knowledge_id, source_path, symbol, source_hash, commit_hash) VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for e in &record.evidence {
            // Validated above, so normalization cannot fail; fall back to the
            // raw value rather than erroring on an impossible path.
            let hash = e
                .source_hash
                .as_deref()
                .map(|h| normalize_source_hash(h).unwrap_or_else(|_| h.to_string()));
            stmt.execute(params![id, e.source_path, e.symbol, hash, e.commit_hash])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// Fetch one record with its evidence (in insertion order), or `None`.
pub fn get_record(conn: &Connection, id: &str) -> Result<Option<KnowledgeRecord>> {
    let record: Option<KnowledgeRecord> = conn
        .query_row(
            "SELECT id, type, title, content, origin, status, created_at, updated_at FROM knowledge WHERE id = ?1",
            params![id],
            |row| {
                let type_: String = row.get(1)?;
                let origin: String = row.get(4)?;
                let status: String = row.get(5)?;
                Ok(KnowledgeRecord {
                    id: row.get(0)?,
                    record_type: KnowledgeType::from_db(&type_),
                    title: row.get(2)?,
                    content: row.get(3)?,
                    origin: KnowledgeOrigin::from_db(&origin),
                    status: KnowledgeStatus::from_db(&status),
                    created_at: row.get(6)?,
                    updated_at: row.get(7)?,
                    evidence: Vec::new(),
                })
            },
        )
        .optional()?;
    let Some(mut record) = record else {
        return Ok(None);
    };
    record.evidence = read_evidence(conn, &record.id)?;
    Ok(Some(record))
}

/// List every record ordered by id, each with its evidence.
pub fn list_records(conn: &Connection) -> Result<Vec<KnowledgeRecord>> {
    let mut stmt = conn.prepare("SELECT id FROM knowledge ORDER BY id")?;
    let ids: Vec<String> = stmt
        .query_map([], |row| row.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    let mut out = Vec::with_capacity(ids.len());
    for id in &ids {
        // Rows are read back-to-back in one connection; a concurrent delete
        // between the id scan and the fetch simply skips the record.
        if let Some(record) = get_record(conn, id)? {
            out.push(record);
        }
    }
    Ok(out)
}

/// Default hits for [`search_records`].
pub const DEFAULT_SEARCH_LIMIT: usize = 20;
/// Maximum hits for [`search_records`].
pub const MAX_SEARCH_LIMIT: usize = 200;

/// Deterministic substring search over id, title, and content
/// (case-insensitive, Unicode-aware).
///
/// Ranking: exact id match, id prefix, id substring, title substring,
/// content substring; ties break by id. Matching runs app-side over the
/// full table rather than `LIKE`: knowledge bases are small (tens to low
/// thousands of rows), and a scan keeps non-ASCII case folding and
/// wildcard-containing queries (`100%`, `a_b`) correct without escape bugs.
/// `limit` defaults to 20 and clamps to 1..=200. Empty queries match nothing.
pub fn search_records(
    conn: &Connection,
    query: &str,
    limit: Option<usize>,
) -> Result<Vec<KnowledgeRecord>> {
    let limit = limit.unwrap_or(DEFAULT_SEARCH_LIMIT).clamp(1, MAX_SEARCH_LIMIT);
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Ok(Vec::new());
    }
    let mut ranked: Vec<(u8, KnowledgeRecord)> = Vec::new();
    // `list_records` orders by id, so the rank sort below is stable on ties.
    for record in list_records(conn)? {
        let id = record.id.to_lowercase();
        let title = record.title.to_lowercase();
        let content = record.content.to_lowercase();
        let rank = if id == q {
            0
        } else if id.starts_with(&q) {
            1
        } else if id.contains(&q) {
            2
        } else if title.contains(&q) {
            3
        } else if content.contains(&q) {
            4
        } else {
            continue;
        };
        ranked.push((rank, record));
    }
    ranked.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.id.cmp(&b.1.id)));
    ranked.truncate(limit);
    Ok(ranked.into_iter().map(|(_, r)| r).collect())
}

/// Records whose evidence cites `path` and/or `symbol` (either filter may be
/// `None`; both `None` matches nothing). Ordered by id. Powers explanations
/// and any future "what do we know about X" views.
pub fn records_linked(
    conn: &Connection,
    path: Option<&str>,
    symbol: Option<&str>,
) -> Result<Vec<KnowledgeRecord>> {
    if path.is_none() && symbol.is_none() {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT DISTINCT knowledge_id FROM knowledge_evidence WHERE (?1 IS NULL OR source_path = ?1) AND (?2 IS NULL OR symbol = ?2) ORDER BY knowledge_id",
    )?;
    let ids: Vec<String> = stmt
        .query_map(params![path, symbol], |row| row.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    let mut out = Vec::with_capacity(ids.len());
    for id in &ids {
        if let Some(record) = get_record(conn, id)? {
            out.push(record);
        }
    }
    Ok(out)
}

/// Evidence rows for one record, in insertion order. Empty strings read back
/// as `None` so blank legacy values never surface as meaningful pins.
fn read_evidence(conn: &Connection, id: &str) -> Result<Vec<KnowledgeEvidence>> {
    let mut stmt = conn.prepare(
        "SELECT source_path, symbol, source_hash, commit_hash FROM knowledge_evidence WHERE knowledge_id = ?1 ORDER BY rowid",
    )?;
    let rows = stmt.query_map(params![id], |row| {
        Ok(KnowledgeEvidence {
            source_path: row.get(0)?,
            symbol: none_if_blank(row.get::<_, Option<String>>(1)?),
            source_hash: none_if_blank(row.get::<_, Option<String>>(2)?),
            commit_hash: none_if_blank(row.get::<_, Option<String>>(3)?),
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

fn none_if_blank(value: Option<String>) -> Option<String> {
    match value {
        Some(s) if s.trim().is_empty() => None,
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;

    fn memory_db() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        schema::initialize(&conn).expect("init schema");
        conn
    }

    fn sample(id: &str, origin: KnowledgeOrigin) -> KnowledgeRecord {
        KnowledgeRecord {
            id: id.to_string(),
            record_type: KnowledgeType::Fact,
            title: "JWT validation".to_string(),
            content: "JWT validation occurs in AuthMiddleware".to_string(),
            origin,
            status: KnowledgeStatus::Unverified,
            created_at: String::new(),
            updated_at: String::new(),
            evidence: vec![KnowledgeEvidence {
                source_path: "src/auth/middleware.ts".to_string(),
                symbol: Some("validateToken".to_string()),
                source_hash: Some(format!("sha256:{}", "A".repeat(64))),
                commit_hash: Some("abc123".to_string()),
            }],
        }
    }

    #[test]
    fn put_get_roundtrip_stamps_and_normalizes() {
        let mut conn = memory_db();
        // Caller timestamps are ignored: the store owns them.
        put_record(&mut conn, &sample("auth.jwt", KnowledgeOrigin::Agent)).unwrap();
        let got = get_record(&conn, "auth.jwt").unwrap().expect("stored record");
        assert_eq!(got.id, "auth.jwt");
        assert_eq!(got.record_type, KnowledgeType::Fact);
        assert_eq!(got.title, "JWT validation");
        assert_eq!(got.origin, KnowledgeOrigin::Agent);
        assert_eq!(got.status, KnowledgeStatus::Unverified);
        assert!(!got.created_at.is_empty());
        assert!(!got.updated_at.is_empty());
        assert_eq!(got.evidence.len(), 1);
        let e = &got.evidence[0];
        assert_eq!(e.source_path, "src/auth/middleware.ts");
        assert_eq!(e.symbol.as_deref(), Some("validateToken"));
        // Prefixed uppercase input is stored as raw lowercase hex.
        assert_eq!(e.source_hash.as_deref(), Some("a".repeat(64).as_str()));
        assert_eq!(e.commit_hash.as_deref(), Some("abc123"));
    }

    #[test]
    fn get_unknown_id_returns_none() {
        let conn = memory_db();
        assert!(get_record(&conn, "nope").unwrap().is_none());
    }

    #[test]
    fn update_preserves_created_at_and_replaces_evidence() {
        let mut conn = memory_db();
        put_record(&mut conn, &sample("k", KnowledgeOrigin::Agent)).unwrap();
        let before = get_record(&conn, "k").unwrap().unwrap();

        let mut next = sample("k", KnowledgeOrigin::Agent);
        next.content = "Updated claim".to_string();
        next.status = KnowledgeStatus::Supported;
        next.evidence.clear();
        put_record(&mut conn, &next).unwrap();

        let after = get_record(&conn, "k").unwrap().unwrap();
        assert_eq!(after.created_at, before.created_at);
        assert!(after.updated_at >= before.updated_at);
        assert_eq!(after.content, "Updated claim");
        assert_eq!(after.status, KnowledgeStatus::Supported);
        assert!(after.evidence.is_empty());
    }

    #[test]
    fn human_records_reject_non_human_overwrites() {
        let mut conn = memory_db();
        put_record(&mut conn, &sample("adr", KnowledgeOrigin::Human)).unwrap();
        for origin in [
            KnowledgeOrigin::Agent,
            KnowledgeOrigin::Inferred,
            KnowledgeOrigin::Deterministic,
            KnowledgeOrigin::Other("bot".to_string()),
        ] {
            let err = put_record(&mut conn, &sample("adr", origin)).unwrap_err();
            match err {
                KeelError::KnowledgeProvenance { id, .. } => assert_eq!(id, "adr"),
                other => panic!("unexpected error: {other}"),
            }
        }
        // The human original is untouched.
        let kept = get_record(&conn, "adr").unwrap().unwrap();
        assert_eq!(kept.origin, KnowledgeOrigin::Human);

        // Human writes to human records are fine.
        let mut human = sample("adr", KnowledgeOrigin::Human);
        human.content = "Human revision".to_string();
        put_record(&mut conn, &human).unwrap();
        assert_eq!(
            get_record(&conn, "adr").unwrap().unwrap().content,
            "Human revision"
        );
    }

    #[test]
    fn non_human_records_accept_human_and_same_origin_updates() {
        let mut conn = memory_db();
        put_record(&mut conn, &sample("k", KnowledgeOrigin::Agent)).unwrap();
        // A human adopting (or correcting) an agent claim is allowed.
        put_record(&mut conn, &sample("k", KnowledgeOrigin::Human)).unwrap();
        assert_eq!(
            get_record(&conn, "k").unwrap().unwrap().origin,
            KnowledgeOrigin::Human
        );
    }

    #[test]
    fn put_rejects_invalid_records() {
        let mut conn = memory_db();
        let mut bad = sample("k", KnowledgeOrigin::Agent);
        bad.id = "has space".to_string();
        assert!(matches!(
            put_record(&mut conn, &bad),
            Err(KeelError::KnowledgeValidation(_))
        ));
        assert!(get_record(&conn, "k").unwrap().is_none());
    }

    #[test]
    fn search_ranks_exact_prefix_substring_title_content() {
        let mut conn = memory_db();
        let mut content_hit = sample("z.content", KnowledgeOrigin::Agent);
        content_hit.title = "Unrelated".to_string();
        content_hit.content = "mentions jwt somewhere".to_string();
        put_record(&mut conn, &content_hit).unwrap();
        let mut title_hit = sample("z.title", KnowledgeOrigin::Agent);
        title_hit.title = "JWT handling".to_string();
        title_hit.content = "nothing relevant".to_string();
        put_record(&mut conn, &title_hit).unwrap();
        put_record(&mut conn, &sample("my.jwt.store", KnowledgeOrigin::Agent)).unwrap();
        put_record(&mut conn, &sample("jwt.suffix", KnowledgeOrigin::Agent)).unwrap();
        put_record(&mut conn, &sample("jwt", KnowledgeOrigin::Agent)).unwrap();

        let ids: Vec<String> = search_records(&conn, "JWT", None)
            .unwrap()
            .iter()
            .map(|r| r.id.clone())
            .collect();
        assert_eq!(
            ids,
            vec![
                "jwt",
                "jwt.suffix",
                "my.jwt.store",
                "z.title",
                "z.content",
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
        );
    }

    #[test]
    fn search_treats_wildcards_literally_and_clamps_limit() {
        let mut conn = memory_db();
        let mut pct = sample("pct", KnowledgeOrigin::Agent);
        pct.content = "100% coverage".to_string();
        put_record(&mut conn, &pct).unwrap();
        let mut under = sample("under", KnowledgeOrigin::Agent);
        under.content = "a_b literal".to_string();
        put_record(&mut conn, &under).unwrap();

        // `%`/`_` are literal text, not wildcards.
        let ids: Vec<String> = search_records(&conn, "100%", None)
            .unwrap()
            .iter()
            .map(|r| r.id.clone())
            .collect();
        assert_eq!(ids, vec!["pct".to_string()]);
        let ids: Vec<String> = search_records(&conn, "a_b", None)
            .unwrap()
            .iter()
            .map(|r| r.id.clone())
            .collect();
        assert_eq!(ids, vec!["under".to_string()]);

        assert!(search_records(&conn, "   ", None).unwrap().is_empty());
        assert!(search_records(&conn, "no-match-xyz", None).unwrap().is_empty());
        // Limit clamps to 1..=200.
        assert_eq!(search_records(&conn, "a", Some(0)).unwrap().len(), 1);
        for i in 0..25 {
            put_record(&mut conn, &sample(&format!("bulk.{i:02}"), KnowledgeOrigin::Agent))
                .unwrap();
        }
        assert_eq!(search_records(&conn, "bulk", None).unwrap().len(), 20);
        assert_eq!(search_records(&conn, "bulk", Some(5)).unwrap().len(), 5);
    }

    #[test]
    fn records_linked_filters_by_path_and_symbol() {
        let mut conn = memory_db();
        // sample() evidence: src/auth/middleware.ts + validateToken.
        put_record(&mut conn, &sample("b.second", KnowledgeOrigin::Agent)).unwrap();
        let mut other = sample("a.first", KnowledgeOrigin::Agent);
        other.evidence[0].source_path = "src/other.ts".to_string();
        other.evidence[0].symbol = Some("other_fn".to_string());
        put_record(&mut conn, &other).unwrap();

        let by_path: Vec<String> = records_linked(&conn, Some("src/auth/middleware.ts"), None)
            .unwrap()
            .iter()
            .map(|r| r.id.clone())
            .collect();
        assert_eq!(by_path, vec!["b.second".to_string()]);

        let by_symbol: Vec<String> = records_linked(&conn, None, Some("validateToken"))
            .unwrap()
            .iter()
            .map(|r| r.id.clone())
            .collect();
        assert_eq!(by_symbol, vec!["b.second".to_string()]);

        // Both filters must hold on the same evidence row.
        assert!(records_linked(&conn, Some("src/other.ts"), Some("validateToken"))
            .unwrap()
            .is_empty());
        assert!(records_linked(&conn, None, None).unwrap().is_empty());
    }

    #[test]
    fn list_returns_records_ordered_by_id() {
        let mut conn = memory_db();
        put_record(&mut conn, &sample("b.second", KnowledgeOrigin::Agent)).unwrap();
        put_record(&mut conn, &sample("a.first", KnowledgeOrigin::Human)).unwrap();
        let ids: Vec<String> = list_records(&conn)
            .unwrap()
            .iter()
            .map(|r| r.id.clone())
            .collect();
        assert_eq!(ids, vec!["a.first".to_string(), "b.second".to_string()]);
    }
}
