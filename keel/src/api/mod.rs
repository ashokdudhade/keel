//! JSON HTTP API exposing symbol intelligence from a Keel index.

use crate::db::{self, queries, schema};
use crate::error::{Result, KeelError};
use crate::graph::deps::{self, Dependency};
use crate::graph::resolve;
use crate::graph::types::{ImplRecord, Reference, Symbol};
use rusqlite::Connection;
use serde::Serialize;
use std::io::Cursor;
use std::path::Path;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

/// Serializable definition location (paths as strings).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SymbolDto {
    /// Symbol identifier.
    pub name: String,
    /// Kind label as stored in the database (e.g. `struct`).
    pub kind: String,
    /// Defining file path.
    pub file: String,
    /// 1-based start line.
    pub start_line: u32,
    /// 1-based start column.
    pub start_col: u32,
    /// Fully-qualified module path.
    pub module_path: String,
}

/// Serializable reference / caller site.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ReferenceDto {
    /// Referenced name.
    pub name: String,
    /// File containing the reference.
    pub file: String,
    /// 1-based start line.
    pub start_line: u32,
    /// 1-based start column.
    pub start_col: u32,
    /// Reference kind label (e.g. `call`).
    pub kind: String,
    /// Enclosing container name.
    pub container: String,
}

/// Serializable trait implementation record.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ImplDto {
    /// Type that implements the trait.
    pub type_name: String,
    /// Trait name, when this is a trait impl.
    pub trait_name: Option<String>,
    /// File containing the impl.
    pub file: String,
    /// 1-based start line.
    pub start_line: u32,
    /// 1-based start column.
    pub start_col: u32,
}

/// Serializable dependency edge.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DependencyDto {
    /// Qualified module path of the dependency.
    pub module_path: String,
    /// Defining file when known.
    pub file: Option<String>,
    /// True when no defining file is indexed (stdlib / third-party).
    pub external: bool,
}

/// Aggregate JSON payload for `GET /symbol/{name}`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SymbolResponse {
    /// Definitions of the name (ordered).
    pub definition: Vec<SymbolDto>,
    /// Reference sites (ordered).
    pub references: Vec<ReferenceDto>,
    /// Trait implementations when `name` is a trait (ordered).
    pub implementations: Vec<ImplDto>,
    /// Modules/files the name depends on (ordered).
    pub dependencies: Vec<DependencyDto>,
    /// Call/use sites of the name (ordered).
    pub callers: Vec<ReferenceDto>,
}

/// Health-check payload for `GET /health`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct HealthResponse {
    /// Always `"ok"` when the server is serving.
    pub status: String,
}

/// Dashboard payload for `GET /api/insights`.
#[derive(Debug, Clone, Serialize)]
pub struct InsightsResponse {
    /// Project identity.
    pub project: InsightsProject,
    /// Live index stats.
    pub index: InsightsIndex,
    /// Usage aggregates over `.keel/usage.jsonl`.
    pub usage: crate::usage::UsageRollup,
    /// Most-queried targets.
    pub top_symbols: Vec<TopSymbol>,
    /// Health checks (`doctor` projection).
    pub health: Vec<HealthCheck>,
    /// Hourly surface counts for the last 7 days, oldest first.
    pub hourly: Vec<crate::usage::HourlyBucket>,
    /// Most recent events (oldest first, capped at 20).
    pub recent: Vec<crate::usage::UsageEvent>,
    /// `on`, `off` (env opt-out), or `empty` (no events yet).
    pub collection: String,
}

/// Project identity for insights.
#[derive(Debug, Clone, Serialize)]
pub struct InsightsProject {
    /// Project root (derived from the index location).
    pub root: String,
    /// Index path as served.
    pub index_db: String,
}

/// Live index stats for insights.
#[derive(Debug, Clone, Serialize)]
pub struct InsightsIndex {
    /// Indexed files.
    pub files: i64,
    /// Indexed symbols.
    pub symbols: i64,
    /// Indexed references.
    pub references: i64,
    /// Writer content-format stamp (0 when legacy).
    pub format: i64,
    /// Whether the stamp matches this build.
    pub format_current: bool,
    /// Writer Keel version, when stamped.
    pub writer: Option<String>,
    /// Last completed index pass (unix seconds, 0 when never).
    pub last_indexed: u64,
}

/// One most-queried target.
#[derive(Debug, Clone, Serialize)]
pub struct TopSymbol {
    /// Queried name.
    pub name: String,
    /// Query count.
    pub n: usize,
}

/// One health check (`doctor` projection).
#[derive(Debug, Clone, Serialize)]
pub struct HealthCheck {
    /// Short check name.
    pub name: String,
    /// False when the user should act.
    pub ok: bool,
    /// One-line status with the fix when `!ok`.
    pub detail: String,
}

impl From<&Symbol> for SymbolDto {
    fn from(s: &Symbol) -> Self {
        Self {
            name: s.name.clone(),
            kind: s.kind.as_db(),
            file: path_string(&s.file),
            start_line: s.start_line,
            start_col: s.start_col,
            module_path: s.module_path.clone(),
        }
    }
}

impl From<&Reference> for ReferenceDto {
    fn from(r: &Reference) -> Self {
        Self {
            name: r.name.clone(),
            file: path_string(&r.file),
            start_line: r.start_line,
            start_col: r.start_col,
            kind: r.kind.as_db(),
            container: r.container.clone(),
        }
    }
}

impl From<&ImplRecord> for ImplDto {
    fn from(i: &ImplRecord) -> Self {
        Self {
            type_name: i.type_name.clone(),
            trait_name: i.trait_name.clone(),
            file: path_string(&i.file),
            start_line: i.start_line,
            start_col: i.start_col,
        }
    }
}

impl From<&Dependency> for DependencyDto {
    fn from(d: &Dependency) -> Self {
        Self {
            module_path: d.module_path.clone(),
            file: d.file.as_ref().map(|p| path_string(p)),
            external: d.external,
        }
    }
}

fn path_string(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Serve the JSON API bound to `addr` (e.g. `127.0.0.1:7645`), reading the
/// index at `db_path`. When `auto_index` is true, each `/symbol` request runs
/// a fast incremental index of the project root first. Blocks until the server
/// stops accepting connections.
pub fn serve(addr: &str, db_path: &Path, auto_index: bool) -> Result<()> {
    let server = Server::http(addr).map_err(|e| KeelError::Api(e.to_string()))?;
    for request in server.incoming_requests() {
        handle_request(request, db_path, auto_index);
    }
    Ok(())
}

/// Always responds; never drops a [`Request`] without a response body.
fn handle_request(request: Request, db_path: &Path, auto_index: bool) {
    let method = request.method().clone();
    let url = request.url().to_string();

    let (status, body, content_type) =
        match build_response(method, &url, db_path, auto_index) {
            Ok(ok) => ok,
            Err(e) => {
                eprintln!("api request error: {e}");
                (
                    StatusCode(500),
                    format!(r#"{{"error":{}}}"#, json_string(&e.to_string())),
                    "application/json",
                )
            }
        };

    if let Err(e) = respond(request, status, &body, content_type) {
        eprintln!("api respond error: {e}");
    }
}

fn build_response(
    method: Method,
    url: &str,
    db_path: &Path,
    auto_index: bool,
) -> Result<(StatusCode, String, &'static str)> {
    if method != Method::Get {
        return Ok((
            StatusCode(405),
            r#"{"error":"method not allowed"}"#.to_string(),
            "application/json",
        ));
    }

    let path = strip_query_fragment(url);

    if path == "/health" {
        let body = serde_json::to_string(&HealthResponse {
            status: "ok".to_string(),
        })
        .map_err(|e| KeelError::Api(e.to_string()))?;
        return Ok((StatusCode(200), body, "application/json"));
    }

    if path == "/insights" {
        return Ok((
            StatusCode(200),
            INSIGHTS_HTML.to_string(),
            "text/html; charset=utf-8",
        ));
    }

    if path == "/api/insights" {
        let payload = insights_payload(db_path)?;
        let body =
            serde_json::to_string(&payload).map_err(|e| KeelError::Api(e.to_string()))?;
        return Ok((StatusCode(200), body, "application/json"));
    }

    if let Some(name) = path.strip_prefix("/symbol/") {
        let name = percent_decode(name);
        if name.is_empty() {
            return Ok((
                StatusCode(400),
                r#"{"error":"missing symbol name"}"#.to_string(),
                "application/json",
            ));
        }
        let payload = symbol_intelligence(db_path, &name, auto_index)?;
        let body =
            serde_json::to_string(&payload).map_err(|e| KeelError::Api(e.to_string()))?;
        return Ok((StatusCode(200), body, "application/json"));
    }

    Ok((
        StatusCode(404),
        r#"{"error":"not found"}"#.to_string(),
        "application/json",
    ))
}

/// Embedded Insights dashboard page (zero dependencies, offline-safe).
const INSIGHTS_HTML: &str = include_str!("insights.html");

/// Build the `/api/insights` payload: live index stats plus usage rollups.
fn insights_payload(db_path: &Path) -> Result<InsightsResponse> {
    let conn = Connection::open(db_path)?;
    db::configure_connection(&conn)?;
    schema::initialize(&conn)?;

    let count = |table: &str| -> i64 {
        conn.query_row(
            &format!("SELECT COUNT(*) FROM {table}"),
            [],
            |row| row.get(0),
        )
        .unwrap_or(0)
    };
    let format = schema::index_format_version(&conn);
    let root = crate::cli::commands::index_root_from_db(db_path);
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let events = crate::usage::read_events(db_path);
    let recent: Vec<crate::usage::UsageEvent> =
        events.iter().rev().take(20).rev().cloned().collect();
    let collection = if std::env::var_os(crate::usage::NO_USAGE_LOG_ENV).is_some() {
        "off"
    } else if events.is_empty() {
        "empty"
    } else {
        "on"
    }
    .to_string();

    Ok(InsightsResponse {
        project: InsightsProject {
            root: root.display().to_string(),
            index_db: db_path.display().to_string(),
        },
        index: InsightsIndex {
            files: count("files"),
            symbols: count("symbols"),
            references: count("\"references\""),
            format,
            format_current: format == schema::INDEX_FORMAT_VERSION,
            writer: schema::writer_version(&conn),
            last_indexed: schema::last_indexed(&conn),
        },
        usage: crate::usage::rollup(&events),
        hourly: crate::usage::hourly(&events, 24 * 7, crate::usage::unix_now()),
        top_symbols: crate::usage::top_targets(&events, 10)
            .into_iter()
            .map(|(name, n)| TopSymbol { name, n })
            .collect(),
        health: crate::daemon::doctor_checks(&root)
            .into_iter()
            .map(|c| HealthCheck {
                name: c.name.to_string(),
                ok: c.ok,
                detail: c.detail,
            })
            .collect(),
        recent,
        collection,
    })
}

fn symbol_intelligence(db_path: &Path, name: &str, auto_index: bool) -> Result<SymbolResponse> {
    let mut conn = Connection::open(db_path)?;
    db::configure_connection(&conn)?;
    schema::initialize(&conn)?;
    if auto_index {
        let root = crate::cli::commands::index_root_from_db(db_path);
        let stats = crate::index::index_repository(&root, &mut conn)?;
        if stats.indexed + stats.removed + stats.errors > 0 {
            eprintln!(
                "keel: auto-indexed {} file(s) (skipped {}, removed {}, errors {}).",
                stats.indexed, stats.skipped, stats.removed, stats.errors
            );
        }
    }
    crate::facade::ensure_index_current(&conn)?;
    let start = std::time::Instant::now();

    let definition = queries::find_definition(&conn, name)?;
    let references = queries::find_references(&conn, name)?;
    let implementations = queries::find_implementations(&conn, name)?;
    let dependencies = deps::find_dependencies(&conn, name)?;
    let target_module = unique_module(&definition);
    let callers = resolve::find_callers(&conn, name, target_module.as_deref())?;

    let response = SymbolResponse {
        definition: definition.iter().map(SymbolDto::from).collect(),
        references: references.iter().map(ReferenceDto::from).collect(),
        implementations: implementations.iter().map(ImplDto::from).collect(),
        dependencies: dependencies.iter().map(DependencyDto::from).collect(),
        callers: callers.iter().map(ReferenceDto::from).collect(),
    };
    // The aggregate endpoint has no trust envelope: log counts only.
    let hits = response.definition.len()
        + response.references.len()
        + response.implementations.len()
        + response.dependencies.len()
        + response.callers.len();
    crate::usage::log_query(
        &conn,
        crate::usage::Surface::Http,
        "symbol",
        name,
        None,
        &crate::usage::QuerySummary::unknown(hits),
        start.elapsed().as_millis() as u64,
    );
    Ok(response)
}

fn unique_module(defs: &[Symbol]) -> Option<String> {
    let first = defs.first()?.module_path.clone();
    if defs.iter().all(|d| d.module_path == first) {
        Some(first)
    } else {
        None
    }
}

fn respond(request: Request, status: StatusCode, body: &str, content_type: &str) -> Result<()> {
    let header = Header::from_bytes("Content-Type", content_type)
        .map_err(|_| KeelError::Api("invalid Content-Type header".into()))?;
    let response = Response::new(
        status,
        vec![header],
        Cursor::new(body.as_bytes().to_vec()),
        Some(body.len()),
        None,
    );
    request
        .respond(response)
        .map_err(|e| KeelError::Api(e.to_string()))
}

/// Strip `?query` and `#fragment` from a URL path.
fn strip_query_fragment(url: &str) -> &str {
    let without_fragment = url.split('#').next().unwrap_or(url);
    without_fragment.split('?').next().unwrap_or(without_fragment)
}

fn json_string(s: &str) -> String {
    match serde_json::to_string(s) {
        Ok(v) => v,
        Err(_) => {
            let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
            format!("\"{escaped}\"")
        }
    }
}

/// Decode a single path segment (`%20` → space). Leaves unknown escapes intact.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_query_and_fragment_from_symbol_path() {
        assert_eq!(
            strip_query_fragment("/symbol/Foo?x=1#frag"),
            "/symbol/Foo"
        );
        assert_eq!(strip_query_fragment("/symbol/Bar"), "/symbol/Bar");
    }
}
