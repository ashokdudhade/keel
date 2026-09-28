//! Append-only per-project usage telemetry for the Insights portal.
//!
//! Every query appends one JSON object per line to `.keel/usage.jsonl`,
//! colocated with the `index.db` that served it. The portal (`keel serve`
//! → `/insights`) rolls these events up; nothing ever leaves the machine.
//!
//! Collection is best-effort and never fails a query: all entry points
//! swallow I/O errors. `KEEL_NO_USAGE_LOG=1` disables writes entirely, and
//! in-memory databases (no backing file) are skipped.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Rotation threshold for `usage.jsonl` (older generation kept as `.1`).
#[cfg(not(test))]
const MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
// Small threshold under test so rotation is exercisable without megabytes.
#[cfg(test)]
const MAX_LOG_BYTES: u64 = 256;

/// Env var that disables usage collection when set (any value).
pub const NO_USAGE_LOG_ENV: &str = "KEEL_NO_USAGE_LOG";

/// Where a logged query came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Cli,
    Mcp,
    Http,
}

impl Surface {
    fn as_str(self) -> &'static str {
        match self {
            Surface::Cli => "cli",
            Surface::Mcp => "mcp",
            Surface::Http => "http",
        }
    }
}

/// Owned summary of a query result envelope for logging.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuerySummary {
    /// Number of result hits.
    pub hits: usize,
    /// `high` / `medium` / `low` (or `unknown` when the surface has no envelope).
    pub confidence: String,
    /// `0` / `1` / `2` / `3` / `mixed` (or `unknown`).
    pub tier: String,
    /// True when a did-you-mean note was emitted.
    pub suggested: bool,
    /// True when the empty-index note was emitted.
    pub empty_index: bool,
}

impl QuerySummary {
    /// Summarize a [`crate::graph::query_result::QueryResult`].
    pub fn from_query_result<T>(qr: &crate::graph::query_result::QueryResult<T>) -> Self {
        use crate::graph::query_result::{Confidence, ResolutionTier};
        let confidence = match qr.confidence {
            Confidence::High => "high",
            Confidence::Medium => "medium",
            Confidence::Low => "low",
        }
        .to_string();
        let tier = match &qr.resolution_tier {
            ResolutionTier::Single(n) => n.to_string(),
            ResolutionTier::Mixed => "mixed".to_string(),
        };
        Self {
            hits: qr.results.len(),
            confidence,
            tier,
            suggested: qr.notes.iter().any(|n| n.starts_with("Did you mean")),
            empty_index: qr.notes.iter().any(|n| n.starts_with("Index is empty")),
        }
    }

    /// Summary for surfaces without an envelope (HTTP aggregate today).
    pub fn unknown(hits: usize) -> Self {
        Self {
            hits,
            confidence: "unknown".into(),
            tier: "unknown".into(),
            suggested: false,
            empty_index: false,
        }
    }
}

/// One usage event (schema v1 — all fields stable).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UsageEvent {
    /// Unix seconds when the query completed.
    pub ts: u64,
    /// `cli` | `mcp` | `http`.
    pub surface: String,
    /// Query tool name (`definition`, `callers`, …).
    pub tool: String,
    /// Queried name/module (bare symbol, not the full response).
    pub target: String,
    /// Module argument when provided.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module: Option<String>,
    /// Number of result hits.
    pub hits: usize,
    /// `high` | `medium` | `low` | `unknown`.
    pub confidence: String,
    /// `0` | `1` | `2` | `3` | `mixed` | `unknown`.
    pub tier: String,
    /// A did-you-mean note was emitted.
    #[serde(default)]
    pub suggested: bool,
    /// The empty-index note was emitted.
    #[serde(default)]
    pub empty_index: bool,
    /// Query latency in milliseconds.
    pub ms: u64,
}

/// Log one query event next to the database behind `conn`.
///
/// Resolves the db file via `PRAGMA database_list`; in-memory databases and
/// `KEEL_NO_USAGE_LOG=1` skip silently. Never fails the caller.
pub fn log_query(
    conn: &rusqlite::Connection,
    surface: Surface,
    tool: &str,
    target: &str,
    module: Option<&str>,
    summary: &QuerySummary,
    elapsed_ms: u64,
) {
    if std::env::var_os(NO_USAGE_LOG_ENV).is_some() {
        return;
    }
    let Some(path) = usage_log_path(conn) else {
        return;
    };
    log_query_at(&path, surface, tool, target, module, summary, elapsed_ms);
}

/// Log one query event to an explicit log path (for callers without the
/// connection, e.g. the CLI whose database is always `./.keel/index.db`).
/// Honors [`NO_USAGE_LOG_ENV`]; never fails the caller.
pub fn log_query_at(
    log_path: &Path,
    surface: Surface,
    tool: &str,
    target: &str,
    module: Option<&str>,
    summary: &QuerySummary,
    elapsed_ms: u64,
) {
    if std::env::var_os(NO_USAGE_LOG_ENV).is_some() {
        return;
    }
    let event = UsageEvent {
        ts: unix_now(),
        surface: surface.as_str().to_string(),
        tool: tool.to_string(),
        target: target.to_string(),
        module: module.map(str::to_string),
        hits: summary.hits,
        confidence: summary.confidence.clone(),
        tier: summary.tier.clone(),
        suggested: summary.suggested,
        empty_index: summary.empty_index,
        ms: elapsed_ms,
    };
    if let Ok(line) = serde_json::to_string(&event) {
        let _ = append_rotating(log_path, line.as_bytes());
    }
}

/// Path of the usage log colocated with `db_path` (`<db-dir>/usage.jsonl`).
pub fn log_path_for_db(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("usage.jsonl")
}

/// Read all usage events (current + rotated generation), oldest first.
/// Corrupt lines are skipped; a missing log reads as empty.
pub fn read_events(db_path: &Path) -> Vec<UsageEvent> {
    let mut out = Vec::new();
    let dir = db_path.parent().unwrap_or_else(|| Path::new("."));
    for name in ["usage.jsonl.1", "usage.jsonl"] {
        let text = std::fs::read_to_string(dir.join(name)).unwrap_or_default();
        for line in text.lines() {
            if let Ok(event) = serde_json::from_str::<UsageEvent>(line.trim()) {
                out.push(event);
            }
        }
    }
    out.sort_by_key(|e| e.ts);
    out
}

/// Aggregated usage for `/api/insights`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct UsageRollup {
    /// Total events counted.
    pub events: usize,
    /// Earliest event timestamp (0 when empty).
    pub since_ts: u64,
    /// Events per tool.
    pub by_tool: BTreeMap<String, usize>,
    /// Events per surface.
    pub by_surface: BTreeMap<String, usize>,
    /// Events per confidence label.
    pub by_confidence: BTreeMap<String, usize>,
    /// Empty-result events.
    pub misses: usize,
    /// Misses that carried a suggestion.
    pub recovered: usize,
    /// Events that hit an empty index.
    pub empty_index_hits: usize,
    /// Total query latency in milliseconds.
    pub total_ms: u64,
}

/// Roll events up into portal-ready aggregates.
pub fn rollup(events: &[UsageEvent]) -> UsageRollup {
    let mut rollup = UsageRollup {
        events: events.len(),
        since_ts: events.iter().map(|e| e.ts).min().unwrap_or(0),
        by_tool: BTreeMap::new(),
        by_surface: BTreeMap::new(),
        by_confidence: BTreeMap::new(),
        misses: 0,
        recovered: 0,
        empty_index_hits: 0,
        total_ms: 0,
    };
    for e in events {
        *rollup.by_tool.entry(e.tool.clone()).or_insert(0) += 1;
        *rollup.by_surface.entry(e.surface.clone()).or_insert(0) += 1;
        *rollup
            .by_confidence
            .entry(e.confidence.clone())
            .or_insert(0) += 1;
        if e.hits == 0 {
            rollup.misses += 1;
        }
        if e.suggested {
            rollup.recovered += 1;
        }
        if e.empty_index {
            rollup.empty_index_hits += 1;
        }
        rollup.total_ms += e.ms;
    }
    rollup
}

/// Most-queried targets (`limit` entries, count desc then name asc).
pub fn top_targets(events: &[UsageEvent], limit: usize) -> Vec<(String, usize)> {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for e in events {
        *counts.entry(e.target.as_str()).or_insert(0) += 1;
    }
    let mut ranked: Vec<(String, usize)> = counts
        .into_iter()
        .map(|(name, n)| (name.to_string(), n))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    ranked.truncate(limit);
    ranked
}

/// Resolve `<db-dir>/usage.jsonl` for a connection, if file-backed.
fn usage_log_path(conn: &rusqlite::Connection) -> Option<PathBuf> {
    let file: String = conn
        .query_row("PRAGMA database_list", [], |row| row.get(2))
        .ok()?;
    if file.is_empty() || file == ":memory:" {
        return None;
    }
    Some(log_path_for_db(Path::new(&file)))
}

/// Append a line, rotating past generations at [`MAX_LOG_BYTES`].
fn append_rotating(path: &Path, line: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() + line.len() as u64 + 1 > MAX_LOG_BYTES {
            let prev = path.with_extension("jsonl.1");
            let _ = std::fs::remove_file(&prev);
            let _ = std::fs::rename(path, &prev);
        }
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(line)?;
    file.write_all(b"\n")?;
    Ok(())
}

/// Per-hour event counts, oldest first, covering the last `hours` hours.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HourlyBucket {
    /// Hour start (unix seconds).
    pub ts: u64,
    /// Events per surface in that hour.
    pub cli: usize,
    /// Events per surface in that hour.
    pub mcp: usize,
    /// Events per surface in that hour.
    pub http: usize,
}

/// Bucket `events` into hourly surface counts ending at `now`.
pub fn hourly(events: &[UsageEvent], hours: usize, now: u64) -> Vec<HourlyBucket> {
    let base = now / 3600 * 3600;
    let start = base.saturating_sub((hours.saturating_sub(1) as u64) * 3600);
    let mut buckets: Vec<HourlyBucket> = (0..hours)
        .map(|i| HourlyBucket {
            ts: start + i as u64 * 3600,
            cli: 0,
            mcp: 0,
            http: 0,
        })
        .collect();
    for e in events {
        if e.ts < start || e.ts >= base + 3600 {
            continue;
        }
        let idx = ((e.ts - start) / 3600) as usize;
        let Some(bucket) = buckets.get_mut(idx) else {
            continue;
        };
        match e.surface.as_str() {
            "cli" => bucket.cli += 1,
            "mcp" => bucket.mcp += 1,
            "http" => bucket.http += 1,
            _ => {}
        }
    }
    buckets
}

pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn summary(hits: usize) -> QuerySummary {
        QuerySummary {
            hits,
            confidence: "high".into(),
            tier: "2".into(),
            suggested: false,
            empty_index: false,
        }
    }

    #[test]
    fn log_and_read_round_trip() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(NO_USAGE_LOG_ENV);
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        let conn = rusqlite::Connection::open(&db).unwrap();

        log_query(&conn, Surface::Mcp, "callers", "serve", None, &summary(2), 3);
        log_query(
            &conn,
            Surface::Cli,
            "definition",
            "AuthService",
            Some("crate::auth"),
            &summary(0),
            1,
        );

        assert!(dir.path().join("usage.jsonl").is_file());
        let events = read_events(&db);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].tool, "callers");
        assert_eq!(events[0].surface, "mcp");
        assert_eq!(events[1].module.as_deref(), Some("crate::auth"));
    }

    #[test]
    fn skips_in_memory_and_opt_out() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(NO_USAGE_LOG_ENV);
        let mem = rusqlite::Connection::open_in_memory().unwrap();
        log_query(&mem, Surface::Cli, "definition", "x", None, &summary(0), 0);
        // Nothing to read anywhere; must not panic.

        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        let conn = rusqlite::Connection::open(&db).unwrap();
        std::env::set_var(NO_USAGE_LOG_ENV, "1");
        log_query(&conn, Surface::Cli, "definition", "x", None, &summary(0), 0);
        std::env::remove_var(NO_USAGE_LOG_ENV);
        assert!(!dir.path().join("usage.jsonl").exists());
    }

    #[test]
    fn rotates_past_threshold() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(NO_USAGE_LOG_ENV);
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        let conn = rusqlite::Connection::open(&db).unwrap();

        for i in 0..10 {
            log_query(
                &conn,
                Surface::Cli,
                "definition",
                &format!("symbol_number_{i}_with_padding"),
                None,
                &summary(1),
                1,
            );
        }
        // 256-byte test threshold forces a rotation across 10 events.
        assert!(dir.path().join("usage.jsonl.1").is_file());
        assert!(!read_events(&db).is_empty());
    }

    fn event_at(ts: u64, surface: &str) -> UsageEvent {
        UsageEvent {
            ts,
            surface: surface.into(),
            tool: "definition".into(),
            target: "x".into(),
            module: None,
            hits: 1,
            confidence: "high".into(),
            tier: "2".into(),
            suggested: false,
            empty_index: false,
            ms: 1,
        }
    }

    #[test]
    fn hourly_buckets_by_surface() {
        let now = 1_000_000 / 3600 * 3600 + 60;
        let events = vec![
            event_at(now - 10, "cli"),
            event_at(now - 20, "mcp"),
            event_at(now - 3500, "http"),
            event_at(now - 100_000, "cli"),
        ];
        let buckets = hourly(&events, 3, now);
        assert_eq!(buckets.len(), 3);
        assert_eq!((buckets[2].cli, buckets[2].mcp), (1, 1));
        assert_eq!(buckets[1].http, 1);
        assert_eq!((buckets[0].cli, buckets[0].mcp, buckets[0].http), (0, 0, 0));
    }

    #[test]
    fn rollup_counts_dimensions() {
        let events = vec![
            UsageEvent {
                ts: 10,
                surface: "mcp".into(),
                tool: "callers".into(),
                target: "serve".into(),
                module: None,
                hits: 2,
                confidence: "high".into(),
                tier: "1".into(),
                suggested: false,
                empty_index: false,
                ms: 3,
            },
            UsageEvent {
                ts: 12,
                surface: "cli".into(),
                tool: "definition".into(),
                target: "Nope".into(),
                module: None,
                hits: 0,
                confidence: "high".into(),
                tier: "0".into(),
                suggested: true,
                empty_index: false,
                ms: 1,
            },
        ];
        let rollup = rollup(&events);
        assert_eq!(rollup.events, 2);
        assert_eq!(rollup.since_ts, 10);
        assert_eq!(rollup.by_tool.get("callers"), Some(&1));
        assert_eq!(rollup.by_surface.get("cli"), Some(&1));
        assert_eq!(rollup.by_confidence.get("high"), Some(&2));
        assert_eq!(rollup.misses, 1);
        assert_eq!(rollup.recovered, 1);
        assert_eq!(rollup.total_ms, 4);

        let top = top_targets(&events, 5);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].1, 1);
    }
}
