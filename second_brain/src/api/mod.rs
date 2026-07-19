//! JSON HTTP API exposing symbol intelligence from a SecondBrain index.

use crate::db::{self, queries, schema};
use crate::error::{Result, SecondBrainError};
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
        }
    }
}

fn path_string(path: &std::path::Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Serve the JSON API bound to `addr` (e.g. `127.0.0.1:7645`), reading the
/// index at `db_path`. Blocks until the server stops accepting connections.
pub fn serve(addr: &str, db_path: &Path) -> Result<()> {
    let server = Server::http(addr).map_err(|e| SecondBrainError::Api(e.to_string()))?;
    for request in server.incoming_requests() {
        handle_request(request, db_path);
    }
    Ok(())
}

/// Always responds; never drops a [`Request`] without a response body.
fn handle_request(request: Request, db_path: &Path) {
    let method = request.method().clone();
    let url = request.url().to_string();

    let (status, body) = match build_response(method, &url, db_path) {
        Ok((status, body)) => (status, body),
        Err(e) => {
            eprintln!("api request error: {e}");
            (
                StatusCode(500),
                format!(r#"{{"error":{}}}"#, json_string(&e.to_string())),
            )
        }
    };

    if let Err(e) = respond(request, status, &body, "application/json") {
        eprintln!("api respond error: {e}");
    }
}

fn build_response(method: Method, url: &str, db_path: &Path) -> Result<(StatusCode, String)> {
    if method != Method::Get {
        return Ok((
            StatusCode(405),
            r#"{"error":"method not allowed"}"#.to_string(),
        ));
    }

    let path = strip_query_fragment(url);

    if path == "/health" {
        let body = serde_json::to_string(&HealthResponse {
            status: "ok".to_string(),
        })
        .map_err(|e| SecondBrainError::Api(e.to_string()))?;
        return Ok((StatusCode(200), body));
    }

    if let Some(name) = path.strip_prefix("/symbol/") {
        let name = percent_decode(name);
        if name.is_empty() {
            return Ok((
                StatusCode(400),
                r#"{"error":"missing symbol name"}"#.to_string(),
            ));
        }
        let payload = symbol_intelligence(db_path, &name)?;
        let body =
            serde_json::to_string(&payload).map_err(|e| SecondBrainError::Api(e.to_string()))?;
        return Ok((StatusCode(200), body));
    }

    Ok((StatusCode(404), r#"{"error":"not found"}"#.to_string()))
}

fn symbol_intelligence(db_path: &Path, name: &str) -> Result<SymbolResponse> {
    let conn = Connection::open(db_path)?;
    db::configure_connection(&conn)?;
    schema::initialize(&conn)?;

    let definition = queries::find_definition(&conn, name)?;
    let references = queries::find_references(&conn, name)?;
    let implementations = queries::find_implementations(&conn, name)?;
    let dependencies = deps::find_dependencies(&conn, name)?;
    let target_module = unique_module(&definition);
    let callers = resolve::find_callers(&conn, name, target_module.as_deref())?;

    Ok(SymbolResponse {
        definition: definition.iter().map(SymbolDto::from).collect(),
        references: references.iter().map(ReferenceDto::from).collect(),
        implementations: implementations.iter().map(ImplDto::from).collect(),
        dependencies: dependencies.iter().map(DependencyDto::from).collect(),
        callers: callers.iter().map(ReferenceDto::from).collect(),
    })
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
        .map_err(|_| SecondBrainError::Api("invalid Content-Type header".into()))?;
    let response = Response::new(
        status,
        vec![header],
        Cursor::new(body.as_bytes().to_vec()),
        Some(body.len()),
        None,
    );
    request
        .respond(response)
        .map_err(|e| SecondBrainError::Api(e.to_string()))
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
