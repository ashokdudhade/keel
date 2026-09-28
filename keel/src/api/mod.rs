//! JSON HTTP API exposing symbol intelligence from a Keel index.

use crate::cli::commands::PreviewCache;
use crate::db::{self, queries, schema};
use crate::error::{Result, KeelError};
use crate::graph::deps::{self, Dependency};
use crate::graph::resolve;
use crate::graph::query_result::QueryResult;
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
    /// Source line at the hit (present only when previews were requested).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
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
    /// Qualifier as written for qualified sites (`mcp` in `mcp::serve`,
    /// `db` in `db.get()`); empty when unqualified.
    pub qualifier: String,
    /// Source line at the hit (present only when previews were requested).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
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
    /// Source line at the hit (present only when previews were requested).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
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

/// Aggregate JSON payload for `GET /symbol/{name}[?limit=N]`: each list is
/// capped at `limit` (default 500) with the true total in `notes`.
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
    /// Per-list truncation notes plus any `?limit=` warning.
    #[serde(default)]
    pub notes: Vec<String>,
}

/// Cap one aggregate list at `limit`, recording an honest per-list note
/// with the true total (mirrors [`QueryResult::truncated`]).
fn cap_symbol_list<T>(
    list: &mut Vec<T>,
    list_name: &str,
    limit: usize,
    notes: &mut Vec<String>,
) {
    if list.len() > limit {
        notes.push(format!(
            "{list_name}: showing first {limit} of {} matches; raise ?limit=N for more.",
            list.len()
        ));
        list.truncate(limit);
    }
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
            preview: None,
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
            qualifier: r.qualifier.clone(),
            preview: None,
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
            preview: None,
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
    // Announce only after the bind succeeds: claiming "Serving …" first
    // lies when the port is privileged or taken.
    eprintln!("Serving Keel JSON API on http://{addr}");
    eprintln!("Keel Insights dashboard on http://{addr}/insights");
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

    if let Some(pattern) = path.strip_prefix("/search/") {
        let pattern = percent_decode(pattern);
        if pattern.is_empty() {
            return Ok((
                StatusCode(400),
                r#"{"error":"missing search pattern"}"#.to_string(),
                "application/json",
            ));
        }
        let (limit, limit_note) = query_limit(url);
        let mut payload = search_response(db_path, &pattern, limit, auto_index)?;
        if let Some(note) = limit_note {
            payload.notes.push(note);
        }
        if query_preview(url) {
            let root = crate::cli::commands::index_root_from_db(db_path);
            let mut cache = PreviewCache::new(root);
            attach_symbol_previews(&mut payload.results, &mut cache);
        }
        let body =
            serde_json::to_string(&payload).map_err(|e| KeelError::Api(e.to_string()))?;
        return Ok((StatusCode(200), body, "application/json"));
    }

    if let Some(file) = path.strip_prefix("/outline/") {
        let file = percent_decode(file);
        if file.is_empty() {
            return Ok((
                StatusCode(400),
                r#"{"error":"missing file path"}"#.to_string(),
                "application/json",
            ));
        }
        let (limit, limit_note) = query_hit_limit(url);
        let mut payload = outline_response(db_path, &file, limit, auto_index)?;
        if let Some(note) = limit_note {
            payload.notes.push(note);
        }
        if query_preview(url) {
            let root = crate::cli::commands::index_root_from_db(db_path);
            let mut cache = PreviewCache::new(root);
            attach_symbol_previews(&mut payload.results, &mut cache);
        }
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
        let (limit, limit_note) = query_hit_limit(url);
        let mut payload = symbol_intelligence(db_path, &name, limit, auto_index)?;
        if let Some(note) = limit_note {
            payload.notes.push(note);
        }
        // Previews cover the hit lists; dependencies are module rows
        // without source lines (the CLI likewise scopes `--preview` to hits).
        if query_preview(url) {
            let root = crate::cli::commands::index_root_from_db(db_path);
            let mut cache = PreviewCache::new(root);
            attach_symbol_previews(&mut payload.definition, &mut cache);
            attach_reference_previews(&mut payload.references, &mut cache);
            attach_reference_previews(&mut payload.callers, &mut cache);
            attach_impl_previews(&mut payload.implementations, &mut cache);
        }
        let body =
            serde_json::to_string(&payload).map_err(|e| KeelError::Api(e.to_string()))?;
        return Ok((StatusCode(200), body, "application/json"));
    }

    if let Some(name) = path.strip_prefix("/impact/") {
        let name = percent_decode(name);
        if name.is_empty() {
            return Ok((
                StatusCode(400),
                r#"{"error":"missing symbol name"}"#.to_string(),
                "application/json",
            ));
        }
        let module = query_module(url);
        let (limit, limit_note) = query_hit_limit(url);
        let mut payload = impact_response(db_path, &name, module.as_deref(), limit, auto_index)?;
        if let Some(note) = limit_note {
            payload.notes.push(note);
        }
        if query_preview(url) {
            let root = crate::cli::commands::index_root_from_db(db_path);
            let mut cache = PreviewCache::new(root);
            attach_symbol_previews(&mut payload.results, &mut cache);
        }
        let body =
            serde_json::to_string(&payload).map_err(|e| KeelError::Api(e.to_string()))?;
        return Ok((StatusCode(200), body, "application/json"));
    }

    if let Some(name) = path.strip_prefix("/dependents/") {
        let name = percent_decode(name);
        if name.is_empty() {
            return Ok((
                StatusCode(400),
                r#"{"error":"missing target name"}"#.to_string(),
                "application/json",
            ));
        }
        let (limit, limit_note) = query_hit_limit(url);
        let mut payload = dependents_response(db_path, &name, limit, auto_index)?;
        if let Some(note) = limit_note {
            payload.notes.push(note);
        }
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

fn symbol_intelligence(
    db_path: &Path,
    name: &str,
    limit: usize,
    auto_index: bool,
) -> Result<SymbolResponse> {
    let mut conn = Connection::open(db_path)?;
    db::configure_connection(&conn)?;
    schema::initialize(&conn)?;
    if auto_index {
        let root = crate::cli::commands::index_root_from_db(db_path);
        let stats = crate::index::index_repository(&root, &mut conn)?;
        if stats.indexed + stats.removed + stats.errors > 0 {
            eprintln!(
                "keel: auto-indexed {} file(s) (skipped {}, removed {}, errors {}, syntax errors {}).",
                stats.indexed,
                stats.skipped,
                stats.removed,
                stats.errors,
                stats.syntax_errors
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

    let mut response = SymbolResponse {
        definition: definition.iter().map(SymbolDto::from).collect(),
        references: references.iter().map(ReferenceDto::from).collect(),
        implementations: implementations.iter().map(ImplDto::from).collect(),
        dependencies: dependencies.iter().map(DependencyDto::from).collect(),
        callers: callers.iter().map(ReferenceDto::from).collect(),
        notes: Vec::new(),
    };
    cap_symbol_list(
        &mut response.definition,
        "definition",
        limit,
        &mut response.notes,
    );
    cap_symbol_list(
        &mut response.references,
        "references",
        limit,
        &mut response.notes,
    );
    cap_symbol_list(
        &mut response.implementations,
        "implementations",
        limit,
        &mut response.notes,
    );
    cap_symbol_list(
        &mut response.dependencies,
        "dependencies",
        limit,
        &mut response.notes,
    );
    cap_symbol_list(
        &mut response.callers,
        "callers",
        limit,
        &mut response.notes,
    );
    // The aggregate endpoint has notes but no confidence tier: log counts only.
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

/// `(limit, warning)` from `?limit=N` (default 50, clamped 1..=200).
/// Unparseable or clamped values fall back with an explanatory note so
/// callers can see the request was not honored as written.
fn query_limit(url: &str) -> (usize, Option<String>) {
    parse_limit_param(url, 50, 1, 200)
}

/// `(limit, warning)` from `?limit=N` for hit lists (default 500, max
/// 100000); unparseable or clamped values note the fallback.
fn query_hit_limit(url: &str) -> (usize, Option<String>) {
    use crate::graph::query_result::{DEFAULT_RESULT_LIMIT, MAX_RESULT_LIMIT};
    parse_limit_param(url, DEFAULT_RESULT_LIMIT, 1, MAX_RESULT_LIMIT)
}

fn parse_limit_param(url: &str, default: usize, min: usize, max: usize) -> (usize, Option<String>) {
    let query = url.split_once('?').map(|(_, q)| q).unwrap_or("");
    let mut garbage: Option<String> = None;
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix("limit=") {
            match value.parse::<usize>() {
                Ok(n) => {
                    let clamped = n.clamp(min, max);
                    let warning =
                        (clamped != n).then(|| format!("limit {n} clamped to {clamped}"));
                    return (clamped, warning);
                }
                Err(_) => {
                    if garbage.is_none() {
                        garbage = Some(format!("invalid limit '{value}', using {default}"));
                    }
                }
            }
        }
    }
    match garbage {
        Some(note) => (default, Some(note)),
        None => (default, None),
    }
}

/// Whether `?preview` asks for source lines (`?preview`, `=1`, `=true`,
/// `=yes` count; `=0`/`=false`/absent do not; last occurrence wins).
fn query_preview(url: &str) -> bool {
    let query = url.split_once('?').map(|(_, q)| q).unwrap_or("");
    let mut on = false;
    for pair in query.split('&') {
        if pair == "preview" {
            on = true;
        } else if let Some(value) = pair.strip_prefix("preview=") {
            on = matches!(value, "" | "1" | "true" | "yes");
        }
    }
    on
}

/// Fill `preview` on every symbol hit from the project sources.
fn attach_symbol_previews(hits: &mut [SymbolDto], cache: &mut PreviewCache) {
    for d in hits {
        d.preview = cache.line(Path::new(&d.file), d.start_line);
    }
}

/// Fill `preview` on every reference hit from the project sources.
fn attach_reference_previews(hits: &mut [ReferenceDto], cache: &mut PreviewCache) {
    for d in hits {
        d.preview = cache.line(Path::new(&d.file), d.start_line);
    }
}

/// Fill `preview` on every implementation hit from the project sources.
fn attach_impl_previews(hits: &mut [ImplDto], cache: &mut PreviewCache) {
    for d in hits {
        d.preview = cache.line(Path::new(&d.file), d.start_line);
    }
}

/// `module` from `?module=M` (percent-decoded); empty/absent → `None`.
fn query_module(url: &str) -> Option<String> {
    let query = url.split_once('?').map(|(_, q)| q).unwrap_or("");
    for pair in query.split('&') {
        if let Some(value) = pair.strip_prefix("module=") {
            let decoded = percent_decode(value);
            if !decoded.is_empty() {
                return Some(decoded);
            }
        }
    }
    None
}

/// Impact payload for `GET /impact/{name}[?module=M][&limit=N]`: candidate
/// blast radius with the trust envelope.
fn impact_response(
    db_path: &Path,
    name: &str,
    module: Option<&str>,
    limit: usize,
    auto_index: bool,
) -> Result<QueryResult<SymbolDto>> {
    let mut conn = Connection::open(db_path)?;
    db::configure_connection(&conn)?;
    schema::initialize(&conn)?;
    if auto_index {
        let root = crate::cli::commands::index_root_from_db(db_path);
        let stats = crate::index::index_repository(&root, &mut conn)?;
        if stats.indexed + stats.removed + stats.errors > 0 {
            eprintln!(
                "keel: auto-indexed {} file(s) (skipped {}, removed {}, errors {}, syntax errors {}).",
                stats.indexed,
                stats.skipped,
                stats.removed,
                stats.errors,
                stats.syntax_errors
            );
        }
    }
    let start = std::time::Instant::now();
    let qr = crate::facade::impact_with_meta_opts(&conn, name, module)?
        .truncated(limit)
        .map_results(|s| SymbolDto::from(&s));
    let summary = crate::usage::QuerySummary::from_query_result(&qr);
    crate::usage::log_query(
        &conn,
        crate::usage::Surface::Http,
        "impact",
        name,
        module,
        &summary,
        start.elapsed().as_millis() as u64,
    );
    Ok(qr)
}

/// Dependents payload for `GET /dependents/{target}[?limit=N]`: reverse
/// dependencies with the trust envelope.
fn dependents_response(
    db_path: &Path,
    name: &str,
    limit: usize,
    auto_index: bool,
) -> Result<QueryResult<DependencyDto>> {
    let mut conn = Connection::open(db_path)?;
    db::configure_connection(&conn)?;
    schema::initialize(&conn)?;
    if auto_index {
        let root = crate::cli::commands::index_root_from_db(db_path);
        let stats = crate::index::index_repository(&root, &mut conn)?;
        if stats.indexed + stats.removed + stats.errors > 0 {
            eprintln!(
                "keel: auto-indexed {} file(s) (skipped {}, removed {}, errors {}, syntax errors {}).",
                stats.indexed,
                stats.skipped,
                stats.removed,
                stats.errors,
                stats.syntax_errors
            );
        }
    }
    let start = std::time::Instant::now();
    let qr = crate::facade::dependents_with_meta(&conn, name)?
        .truncated(limit)
        .map_results(|d| DependencyDto::from(&d));
    let summary = crate::usage::QuerySummary::from_query_result(&qr);
    crate::usage::log_query(
        &conn,
        crate::usage::Surface::Http,
        "dependents",
        name,
        None,
        &summary,
        start.elapsed().as_millis() as u64,
    );
    Ok(qr)
}

/// Search payload for `GET /search/{pattern}[?limit=N]`.
fn search_response(
    db_path: &Path,
    pattern: &str,
    limit: usize,
    auto_index: bool,
) -> Result<QueryResult<SymbolDto>> {
    let mut conn = Connection::open(db_path)?;
    db::configure_connection(&conn)?;
    schema::initialize(&conn)?;
    if auto_index {
        let root = crate::cli::commands::index_root_from_db(db_path);
        let stats = crate::index::index_repository(&root, &mut conn)?;
        if stats.indexed + stats.removed + stats.errors > 0 {
            eprintln!(
                "keel: auto-indexed {} file(s) (skipped {}, removed {}, errors {}, syntax errors {}).",
                stats.indexed,
                stats.skipped,
                stats.removed,
                stats.errors,
                stats.syntax_errors
            );
        }
    }
    let start = std::time::Instant::now();
    let qr = crate::facade::search_with_meta(&conn, pattern, limit)?
        .map_results(|s| SymbolDto::from(&s));
    let summary = crate::usage::QuerySummary::from_query_result(&qr);
    crate::usage::log_query(
        &conn,
        crate::usage::Surface::Http,
        "search",
        pattern,
        None,
        &summary,
        start.elapsed().as_millis() as u64,
    );
    Ok(qr)
}

/// Outline payload for `GET /outline/{path}[?limit=N]`: file symbols with
/// the trust envelope.
fn outline_response(
    db_path: &Path,
    file: &str,
    limit: usize,
    auto_index: bool,
) -> Result<QueryResult<SymbolDto>> {
    let mut conn = Connection::open(db_path)?;
    db::configure_connection(&conn)?;
    schema::initialize(&conn)?;
    if auto_index {
        let root = crate::cli::commands::index_root_from_db(db_path);
        let stats = crate::index::index_repository(&root, &mut conn)?;
        if stats.indexed + stats.removed + stats.errors > 0 {
            eprintln!(
                "keel: auto-indexed {} file(s) (skipped {}, removed {}, errors {}, syntax errors {}).",
                stats.indexed,
                stats.skipped,
                stats.removed,
                stats.errors,
                stats.syntax_errors
            );
        }
    }
    let start = std::time::Instant::now();
    let qr = crate::facade::outline_with_meta(&conn, file)?
        .truncated(limit)
        .map_results(|s| SymbolDto::from(&s));
    let summary = crate::usage::QuerySummary::from_query_result(&qr);
    crate::usage::log_query(
        &conn,
        crate::usage::Surface::Http,
        "outline",
        file,
        None,
        &summary,
        start.elapsed().as_millis() as u64,
    );
    Ok(qr)
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
    fn reference_dto_carries_qualifier() {
        use crate::graph::types::{Reference, ReferenceKind};
        let dto = ReferenceDto::from(&Reference {
            name: "serve".to_string(),
            file: std::path::PathBuf::from("src/cli/commands.rs"),
            start_line: 921,
            start_col: 10,
            kind: ReferenceKind::Path,
            container: "crate::cli::commands::run_mcp".to_string(),
            qualifier: "mcp".to_string(),
        });
        assert_eq!(dto.qualifier, "mcp");
        let v = serde_json::to_value(&dto).expect("serialize");
        assert_eq!(v["qualifier"], "mcp");
        assert_eq!(v["container"], "crate::cli::commands::run_mcp");
    }

    #[test]
    fn strip_query_and_fragment_from_symbol_path() {
        assert_eq!(
            strip_query_fragment("/symbol/Foo?x=1#frag"),
            "/symbol/Foo"
        );
        assert_eq!(strip_query_fragment("/symbol/Bar"), "/symbol/Bar");
    }

    fn seeded_db(dir: &std::path::Path) -> std::path::PathBuf {
        use crate::graph::types::{FileNode, Symbol, SymbolKind};
        let db_path = dir.join("test_outline_http.db");
        let conn = Connection::open(&db_path).expect("open db");
        crate::db::configure_connection(&conn).expect("configure");
        crate::db::schema::initialize(&conn).expect("init schema");
        let file_id = crate::db::queries::insert_file(
            &conn,
            &FileNode {
                path: std::path::PathBuf::from("src/lib.rs"),
                content_hash: "h".to_string(),
            },
        )
        .expect("insert file");
        crate::db::queries::insert_symbols(
            &conn,
            file_id,
            &[
                Symbol {
                    name: "alpha".to_string(),
                    kind: SymbolKind::Function,
                    file: std::path::PathBuf::from("src/lib.rs"),
                    start_line: 1,
                    start_col: 1,
                    module_path: String::new(),
                },
                Symbol {
                    name: "beta".to_string(),
                    kind: SymbolKind::Struct,
                    file: std::path::PathBuf::from("src/lib.rs"),
                    start_line: 5,
                    start_col: 1,
                    module_path: String::new(),
                },
            ],
        )
        .expect("insert symbols");
        db_path
    }

    #[test]
    fn outline_route_serves_source_ordered_symbols_with_envelope() {
        let dir =
            std::env::temp_dir().join(format!("keel_outline_http_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let db_path = seeded_db(&dir);
        let (status, body, ctype) =
            build_response(Method::Get, "/outline/src/lib.rs", &db_path, false)
                .expect("build response");
        assert_eq!(status, StatusCode(200));
        assert_eq!(ctype, "application/json");
        let v: serde_json::Value = serde_json::from_str(&body).expect("parse json");
        let names: Vec<&str> = v["results"]
            .as_array()
            .expect("results array")
            .iter()
            .map(|s| s["name"].as_str().expect("name"))
            .collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert!(v.get("confidence").is_some(), "trust envelope present");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn outline_route_honors_limit_query() {
        let dir =
            std::env::temp_dir().join(format!("keel_outline_limit_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let db_path = seeded_db(&dir);
        let (status, body, _) =
            build_response(Method::Get, "/outline/src/lib.rs?limit=1", &db_path, false)
                .expect("build response");
        assert_eq!(status, StatusCode(200));
        let v: serde_json::Value = serde_json::from_str(&body).expect("parse json");
        assert_eq!(
            v["results"].as_array().expect("results array").len(),
            1,
            "limit=1 must cap two symbols, got {body}"
        );
        assert_eq!(v["results"][0]["name"], "alpha");
        let notes = v["notes"].as_array().expect("notes array");
        assert!(
            notes.iter().any(|n| n
                .as_str()
                .unwrap_or("")
                .contains("Showing first 1 of 2 matches")),
            "capped response must carry the true total, got {body}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn symbol_route_caps_each_list_with_per_list_notes() {
        use crate::graph::types::{FileNode, Symbol, SymbolKind};
        let dir =
            std::env::temp_dir().join(format!("keel_symbol_limit_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let db_path = dir.join("test_symbol_http.db");
        {
            let conn = Connection::open(&db_path).expect("open db");
            crate::db::configure_connection(&conn).expect("configure");
            crate::db::schema::initialize(&conn).expect("init schema");
            for path in ["src/a.rs", "src/b.rs"] {
                let file_id = crate::db::queries::insert_file(
                    &conn,
                    &FileNode {
                        path: std::path::PathBuf::from(path),
                        content_hash: "h".to_string(),
                    },
                )
                .expect("insert file");
                crate::db::queries::insert_symbols(
                    &conn,
                    file_id,
                    &[Symbol {
                        name: "dup".to_string(),
                        kind: SymbolKind::Function,
                        file: std::path::PathBuf::from(path),
                        start_line: 1,
                        start_col: 1,
                        module_path: String::new(),
                    }],
                )
                .expect("insert symbols");
            }
        }
        let (status, body, _) =
            build_response(Method::Get, "/symbol/dup?limit=1", &db_path, false)
                .expect("build response");
        assert_eq!(status, StatusCode(200));
        let v: serde_json::Value = serde_json::from_str(&body).expect("parse json");
        assert_eq!(
            v["definition"].as_array().expect("definition array").len(),
            1,
            "limit=1 must cap two definitions, got {body}"
        );
        let notes = v["notes"].as_array().expect("notes array");
        assert!(
            notes.iter().any(|n| n
                .as_str()
                .unwrap_or("")
                .contains("definition: showing first 1 of 2 matches")),
            "capped list must name itself with the true total, got {body}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn search_route_ranks_and_honors_limit_query() {
        let dir =
            std::env::temp_dir().join(format!("keel_search_http_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let db_path = seeded_db(&dir);
        let (status, body, _) =
            build_response(Method::Get, "/search/alpha", &db_path, false)
                .expect("build response");
        assert_eq!(status, StatusCode(200));
        let v: serde_json::Value = serde_json::from_str(&body).expect("parse json");
        assert_eq!(v["results"].as_array().expect("array").len(), 1);
        assert_eq!(v["results"][0]["name"], "alpha");

        let (status, body, _) =
            build_response(Method::Get, "/search/a?limit=1", &db_path, false)
                .expect("build response");
        assert_eq!(status, StatusCode(200));
        let v: serde_json::Value = serde_json::from_str(&body).expect("parse json");
        assert_eq!(v["results"].as_array().expect("array").len(), 1);
        assert!(
            v["notes"].as_array().expect("notes").iter().any(|n| n
                .as_str()
                .unwrap_or("")
                .contains("first 1 matches")),
            "got {}",
            body
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn search_route_rejects_empty_pattern() {
        let dir =
            std::env::temp_dir().join(format!("keel_search_empty_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let db_path = seeded_db(&dir);
        let (status, _, _) = build_response(Method::Get, "/search/", &db_path, false)
            .expect("build response");
        assert_eq!(status, StatusCode(400));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn query_limit_parses_and_clamps() {
        assert_eq!(query_limit("/search/x"), (50, None));
        assert_eq!(query_limit("/search/x?limit=5"), (5, None));
        assert_eq!(query_limit("/search/x?a=1&limit=7"), (7, None));
        // Garbage and clamps fall back with an explanatory note.
        assert_eq!(
            query_limit("/search/x?limit=bogus"),
            (50, Some("invalid limit 'bogus', using 50".to_string()))
        );
        assert_eq!(
            query_limit("/search/x?limit=0"),
            (1, Some("limit 0 clamped to 1".to_string()))
        );
        assert_eq!(
            query_limit("/search/x?limit=9999"),
            (200, Some("limit 9999 clamped to 200".to_string()))
        );
        // A later valid value wins over earlier garbage, silently.
        assert_eq!(query_limit("/search/x?limit=bogus&limit=7"), (7, None));
        assert_eq!(
            query_hit_limit("/impact/x?limit=bogus"),
            (500, Some("invalid limit 'bogus', using 500".to_string()))
        );
    }

    #[test]
    fn query_preview_accepts_flag_and_truthy_values() {
        assert!(!query_preview("/symbol/x"));
        assert!(query_preview("/symbol/x?preview"));
        assert!(query_preview("/symbol/x?preview="));
        assert!(query_preview("/symbol/x?preview=1"));
        assert!(query_preview("/symbol/x?limit=5&preview=true"));
        assert!(!query_preview("/symbol/x?preview=0"));
        assert!(!query_preview("/symbol/x?preview=false"));
        // Last occurrence wins.
        assert!(query_preview("/symbol/x?preview=0&preview=1"));
        assert!(!query_preview("/symbol/x?preview=1&preview=0"));
    }

    #[test]
    fn outline_route_rejects_empty_path() {
        let dir =
            std::env::temp_dir().join(format!("keel_outline_empty_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let db_path = seeded_db(&dir);
        let (status, _, _) = build_response(Method::Get, "/outline/", &db_path, false)
            .expect("build response");
        assert_eq!(status, StatusCode(400));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Two modules: `b` imports `a`.
    fn seeded_dep_db(dir: &std::path::Path) -> std::path::PathBuf {
        use crate::graph::types::{FileNode, Import, Symbol, SymbolKind};
        let db_path = dir.join("test_dep_http.db");
        let conn = Connection::open(&db_path).expect("open db");
        crate::db::configure_connection(&conn).expect("configure");
        crate::db::schema::initialize(&conn).expect("init schema");
        let a_id = crate::db::queries::insert_file(
            &conn,
            &FileNode {
                path: std::path::PathBuf::from("src/a.rs"),
                content_hash: "h".to_string(),
            },
        )
        .expect("insert a");
        crate::db::queries::insert_symbols(
            &conn,
            a_id,
            &[Symbol {
                name: "alpha".to_string(),
                kind: SymbolKind::Function,
                file: std::path::PathBuf::from("src/a.rs"),
                start_line: 1,
                start_col: 1,
                module_path: "a".to_string(),
            }],
        )
        .expect("insert alpha");
        let b_id = crate::db::queries::insert_file(
            &conn,
            &FileNode {
                path: std::path::PathBuf::from("src/b.rs"),
                content_hash: "h".to_string(),
            },
        )
        .expect("insert b");
        crate::db::queries::insert_symbols(
            &conn,
            b_id,
            &[Symbol {
                name: "beta".to_string(),
                kind: SymbolKind::Function,
                file: std::path::PathBuf::from("src/b.rs"),
                start_line: 1,
                start_col: 1,
                module_path: "b".to_string(),
            }],
        )
        .expect("insert beta");
        crate::db::queries::insert_imports(
            &conn,
            b_id,
            &[Import {
                module_path: "a".to_string(),
                alias: None,
                file: std::path::PathBuf::from("src/b.rs"),
            }],
        )
        .expect("insert import");
        db_path
    }

    #[test]
    fn dependents_route_lists_importing_modules_with_envelope() {
        let dir =
            std::env::temp_dir().join(format!("keel_deps_http_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let db_path = seeded_dep_db(&dir);
        let (status, body, ctype) =
            build_response(Method::Get, "/dependents/a", &db_path, false)
                .expect("build response");
        assert_eq!(status, StatusCode(200));
        assert_eq!(ctype, "application/json");
        let v: serde_json::Value = serde_json::from_str(&body).expect("parse json");
        let modules: Vec<&str> = v["results"]
            .as_array()
            .expect("results array")
            .iter()
            .map(|d| d["module_path"].as_str().expect("module_path"))
            .collect();
        assert_eq!(modules, vec!["b"]);
        assert!(v.get("confidence").is_some(), "trust envelope present");
        assert!(v.get("notes").is_some(), "trust envelope present");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dependents_route_honors_limit_query() {
        use crate::graph::types::{FileNode, Import};
        let dir =
            std::env::temp_dir().join(format!("keel_deps_limit_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let db_path = seeded_dep_db(&dir);
        {
            let conn = Connection::open(&db_path).expect("open db");
            let c_id = crate::db::queries::insert_file(
                &conn,
                &FileNode {
                    path: std::path::PathBuf::from("src/c.rs"),
                    content_hash: "h".to_string(),
                },
            )
            .expect("insert c");
            crate::db::queries::insert_imports(
                &conn,
                c_id,
                &[Import {
                    module_path: "a".to_string(),
                    alias: None,
                    file: std::path::PathBuf::from("src/c.rs"),
                }],
            )
            .expect("insert import");
        }
        let (status, body, _) =
            build_response(Method::Get, "/dependents/a?limit=1", &db_path, false)
                .expect("build response");
        assert_eq!(status, StatusCode(200));
        let v: serde_json::Value = serde_json::from_str(&body).expect("parse json");
        assert_eq!(
            v["results"].as_array().expect("results array").len(),
            1,
            "limit=1 must cap two dependents, got {body}"
        );
        let notes = v["notes"].as_array().expect("notes array");
        assert!(
            notes.iter().any(|n| n
                .as_str()
                .unwrap_or("")
                .contains("Showing first 1 of 2 matches")),
            "capped response must carry the true total, got {body}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn impact_route_returns_envelope() {
        let dir =
            std::env::temp_dir().join(format!("keel_impact_http_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let db_path = seeded_dep_db(&dir);
        let (status, body, _) =
            build_response(Method::Get, "/impact/alpha?module=a", &db_path, false)
                .expect("build response");
        assert_eq!(status, StatusCode(200));
        let v: serde_json::Value = serde_json::from_str(&body).expect("parse json");
        assert!(v["results"].is_array(), "results array, got {body}");
        assert!(v.get("confidence").is_some(), "trust envelope present");
        assert!(v.get("notes").is_some(), "trust envelope present");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn impact_and_dependents_routes_reject_empty_target() {
        let dir =
            std::env::temp_dir().join(format!("keel_idep_empty_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        let db_path = seeded_dep_db(&dir);
        for route in ["/impact/", "/dependents/"] {
            let (status, _, _) = build_response(Method::Get, route, &db_path, false)
                .expect("build response");
            assert_eq!(status, StatusCode(400), "route {route}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn query_module_parses_and_decodes() {
        assert_eq!(query_module("/impact/x"), None);
        assert_eq!(
            query_module("/impact/x?module=crate%3A%3Amcp"),
            Some("crate::mcp".to_string())
        );
        assert_eq!(query_module("/impact/x?module="), None);
        assert_eq!(
            query_module("/impact/x?a=1&module=m"),
            Some("m".to_string())
        );
    }
}
