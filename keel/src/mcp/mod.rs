//! MCP stdio server: JSON-RPC 2.0 over stdin/stdout.
//!
//! Supports both wire formats used by MCP clients:
//! - **Newline-delimited JSON** (Cursor 2025-11+): one JSON object per line
//! - **Content-Length framing** (older clients / LSP-style)
//!
//! Logs must go to stderr only — stdout is reserved for JSON-RPC.

use crate::api::{DependencyDto, ImplDto, ReferenceDto, SymbolDto};
use crate::cli::commands::PreviewCache;
use crate::db::schema;
use crate::error::{Result, KeelError};
use crate::facade;
use crate::index::{self, IndexStats};
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{json, Value};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

const PROTOCOL_VERSION: &str = "2024-11-05";
/// Protocol versions we can speak. Prefer echoing the client's request when
/// supported so newer Cursor builds don't reject the handshake.
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &[
    "2025-11-25",
    "2025-06-18",
    "2025-03-26",
    "2024-11-05",
];
const SERVER_NAME: &str = "keel";

/// Wire format negotiated from the first inbound message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireFormat {
    /// One JSON object per line, terminated by `\n`.
    Ndjson,
    /// LSP-style `Content-Length` headers + body.
    ContentLength,
}

/// Encode a JSON-RPC body for the given wire format.
pub fn encode_message_with(format: WireFormat, body: &[u8]) -> Vec<u8> {
    match format {
        WireFormat::Ndjson => {
            let mut out = Vec::with_capacity(body.len() + 1);
            out.extend_from_slice(body);
            out.push(b'\n');
            out
        }
        WireFormat::ContentLength => {
            // Include Content-Type; some clients are picky about framed headers.
            let header = format!(
                "Content-Length: {}\r\nContent-Type: application/json\r\n\r\n",
                body.len()
            );
            let mut out = Vec::with_capacity(header.len() + body.len());
            out.extend_from_slice(header.as_bytes());
            out.extend_from_slice(body);
            out
        }
    }
}

/// Encode a JSON-RPC body as a Content-Length framed MCP message.
pub fn encode_message(body: &[u8]) -> Vec<u8> {
    encode_message_with(WireFormat::ContentLength, body)
}

/// Read one MCP message, detecting NDJSON vs Content-Length on the first call.
///
/// Callers should reuse a single [`BufRead`] across the session so buffered
/// bytes from one frame are not dropped before the next.
pub fn read_message(
    reader: &mut impl BufRead,
    format: &mut Option<WireFormat>,
) -> Result<Vec<u8>> {
    let mut line = String::new();
    let n = reader
        .read_line(&mut line)
        .map_err(|source| KeelError::Io {
            path: PathBuf::from("stdin"),
            source,
        })?;
    if n == 0 {
        return Err(KeelError::Mcp("unexpected EOF while reading headers".into()));
    }

    let trimmed = line.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        // Blank line before headers — continue as Content-Length.
        *format = Some(WireFormat::ContentLength);
        return read_content_length_after_first_line(reader, None);
    }

    // Cursor (protocol 2025-11-25+) sends raw JSON lines with no headers.
    if trimmed.starts_with('{') {
        *format = Some(WireFormat::Ndjson);
        return Ok(trimmed.as_bytes().to_vec());
    }

    *format = Some(WireFormat::ContentLength);
    let mut content_length = None;
    if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
        let len = rest.trim().parse::<usize>().map_err(|_| {
            KeelError::Mcp(format!("invalid Content-Length: {rest:?}"))
        })?;
        content_length = Some(len);
    }
    read_content_length_after_first_line(reader, content_length)
}

fn read_content_length_after_first_line(
    reader: &mut impl BufRead,
    mut content_length: Option<usize>,
) -> Result<Vec<u8>> {
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .map_err(|source| KeelError::Io {
                path: PathBuf::from("stdin"),
                source,
            })?;
        if n == 0 {
            return Err(KeelError::Mcp("unexpected EOF while reading headers".into()));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            let len = rest.trim().parse::<usize>().map_err(|_| {
                KeelError::Mcp(format!("invalid Content-Length: {rest:?}"))
            })?;
            content_length = Some(len);
        }
        // Other headers (e.g. Content-Type) are ignored.
    }

    let len = content_length
        .ok_or_else(|| KeelError::Mcp("missing Content-Length header".into()))?;
    let mut body = vec![0u8; len];
    reader
        .read_exact(&mut body)
        .map_err(|source| KeelError::Io {
            path: PathBuf::from("stdin"),
            source,
        })?;
    Ok(body)
}

/// Dispatch a single JSON-RPC request/notification.
///
/// Returns `Ok(None)` for notifications that need no response.
pub fn handle_message(conn: &mut Connection, msg: &Value) -> Result<Option<Value>> {
    handle_message_with(conn, msg, false, Path::new("."))
}

fn handle_message_with(
    conn: &mut Connection,
    msg: &Value,
    auto_index: bool,
    root: &Path,
) -> Result<Option<Value>> {
    let method = msg
        .get("method")
        .and_then(|m| m.as_str())
        .ok_or_else(|| KeelError::Mcp("missing method".into()))?;
    let id = msg.get("id").cloned();

    // Notifications have no `id` and must not produce a response.
    if id.is_none() {
        match method {
            "notifications/initialized" | "initialized" => return Ok(None),
            other => {
                eprintln!("mcp: ignoring notification {other}");
                return Ok(None);
            }
        }
    }

    let result = match method {
        "initialize" => Ok(initialize_result(msg)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools_list_result()),
        // Some clients probe these even when capabilities omit them.
        // Return empty lists instead of -32601 so tool discovery can finish.
        "resources/list" => Ok(json!({ "resources": [] })),
        "resources/templates/list" => Ok(json!({ "resourceTemplates": [] })),
        "prompts/list" => Ok(json!({ "prompts": [] })),
        "tools/call" => {
            let params = msg.get("params").cloned().unwrap_or(json!({}));
            call_tool(conn, &params, auto_index, root)
        }
        other => {
            return Ok(Some(json_rpc_error(
                id.unwrap_or(Value::Null),
                -32601,
                format!("Method not found: {other}"),
            )));
        }
    };

    match result {
        Ok(value) => Ok(Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": value,
        }))),
        Err(e) => Ok(Some(json_rpc_error(
            id.unwrap_or(Value::Null),
            -32000,
            e.to_string(),
        ))),
    }
}

/// Serve MCP over stdin/stdout using the index at `db_path`.
///
/// Handshake methods (`initialize`, `tools/list`, …) are answered before the
/// SQLite DB is opened so a locked index (e.g. from `keel watch`) cannot stall
/// Cursor's MCP client. The DB is opened lazily on the first tool call.
/// When `auto_index` is true, query tools run a fast incremental index first.
pub fn serve(db_path: &Path, auto_index: bool) -> Result<()> {
    if let Some(parent) = db_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|source| KeelError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
    }
    let root = crate::cli::commands::index_root_from_db(db_path);
    let mut conn: Option<Connection> = None;

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());
    // Detected from the first inbound frame; default NDJSON matches Cursor 2025-11+.
    let mut wire: Option<WireFormat> = None;

    loop {
        let body = match read_message(&mut reader, &mut wire) {
            Ok(b) => b,
            Err(KeelError::Io { source, .. })
                if source.kind() == io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(e) => {
                // Clean EOF on header read also ends the session.
                if e.to_string().contains("unexpected EOF") {
                    break;
                }
                eprintln!("mcp: read error: {e}");
                break;
            }
        };
        let format = wire.unwrap_or(WireFormat::Ndjson);

        let msg: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("mcp: invalid JSON: {e}");
                let err = json_rpc_error(Value::Null, -32700, format!("Parse error: {e}"));
                write_response(&mut stdout, format, &err)?;
                continue;
            }
        };

        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        if needs_db(method) && conn.is_none() {
            match open_index_db(db_path) {
                Ok(c) => conn = Some(c),
                Err(e) => {
                    let id = msg.get("id").cloned().unwrap_or(Value::Null);
                    let err = json_rpc_error(id, -32000, e.to_string());
                    write_response(&mut stdout, format, &err)?;
                    continue;
                }
            }
        }

        let result = match conn.as_mut() {
            Some(c) => handle_message_with(c, &msg, auto_index, &root),
            None => handle_message_without_db(&msg),
        };

        match result {
            Ok(Some(response)) => write_response(&mut stdout, format, &response)?,
            Ok(None) => {}
            Err(e) => {
                eprintln!("mcp: handler error: {e}");
                let id = msg.get("id").cloned().unwrap_or(Value::Null);
                let err = json_rpc_error(id, -32603, e.to_string());
                write_response(&mut stdout, format, &err)?;
            }
        }
    }
    Ok(())
}

fn needs_db(method: &str) -> bool {
    matches!(method, "tools/call")
}

fn open_index_db(db_path: &Path) -> Result<Connection> {
    let conn = Connection::open(db_path)?;
    crate::db::configure_connection(&conn)?;
    schema::initialize(&conn)?;
    Ok(conn)
}

/// Handshake / discovery handlers that must not touch SQLite.
fn handle_message_without_db(msg: &Value) -> Result<Option<Value>> {
    let method = msg
        .get("method")
        .and_then(|m| m.as_str())
        .ok_or_else(|| KeelError::Mcp("missing method".into()))?;
    let id = msg.get("id").cloned();

    if id.is_none() {
        match method {
            "notifications/initialized" | "initialized" => return Ok(None),
            other => {
                eprintln!("mcp: ignoring notification {other}");
                return Ok(None);
            }
        }
    }

    let result: Result<Value> = match method {
        "initialize" => Ok(initialize_result(msg)),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools_list_result()),
        "resources/list" => Ok(json!({ "resources": [] })),
        "resources/templates/list" => Ok(json!({ "resourceTemplates": [] })),
        "prompts/list" => Ok(json!({ "prompts": [] })),
        other => {
            return Ok(Some(json_rpc_error(
                id.unwrap_or(Value::Null),
                -32601,
                format!("Method not found: {other}"),
            )));
        }
    };

    match result {
        Ok(value) => Ok(Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": value,
        }))),
        Err(e) => Ok(Some(json_rpc_error(
            id.unwrap_or(Value::Null),
            -32000,
            e.to_string(),
        ))),
    }
}

fn write_response(
    stdout: &mut impl Write,
    format: WireFormat,
    response: &Value,
) -> Result<()> {
    let body =
        serde_json::to_vec(response).map_err(|e| KeelError::Mcp(e.to_string()))?;
    let framed = encode_message_with(format, &body);
    stdout
        .write_all(&framed)
        .map_err(|source| KeelError::Io {
            path: PathBuf::from("stdout"),
            source,
        })?;
    stdout.flush().map_err(|source| KeelError::Io {
        path: PathBuf::from("stdout"),
        source,
    })?;
    Ok(())
}

fn json_rpc_error(id: Value, code: i64, message: String) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        }
    })
}

fn initialize_result(msg: &Value) -> Value {
    let requested = msg
        .pointer("/params/protocolVersion")
        .and_then(|v| v.as_str())
        .unwrap_or(PROTOCOL_VERSION);
    let protocol_version = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
        requested
    } else {
        // Prefer the newest we know when the client asks for something unknown.
        SUPPORTED_PROTOCOL_VERSIONS[0]
    };
    json!({
        "protocolVersion": protocol_version,
        "capabilities": {
            "tools": {}
        },
        "serverInfo": {
            "name": SERVER_NAME,
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

fn tools_list_result() -> Value {
    let trust = " Returns JSON with results, confidence (high|medium|low), resolution_tier (1=strongest evidence, 3=weakest, mixed, or 0 when nothing resolved), and notes. If confidence is low or notes warn of ambiguity, disambiguate with module / a qualified name (e.g. crate::mcp::serve) or fall back to Grep. Empty results with “No matching symbols found” means a confident miss — try another name, not Grep-for-noise first. Non-empty impact is always a candidate blast radius (medium/low) — verify before edits.";
    json!({
        "tools": [
            tool_def(
                "definition",
                &format!(
                    "Find definition location(s) for a symbol. Prefer exact names; `Type.member` finds the member inside that type. Optional module disambiguates overloads.{trust}"
                ),
                with_preview(capped_query_schema()),
                true,
            ),
            tool_def(
                "references",
                &format!(
                    "Find reference sites for a symbol name, including uses through import aliases. Optional module narrows the defining symbol when names collide.{trust}"
                ),
                with_preview(capped_query_schema()),
                true,
            ),
            tool_def(
                "callers",
                &format!(
                    "Find call/use sites of a function or symbol, including uses through import aliases. Import-aware when the definition module is unique or provided.{trust}"
                ),
                with_preview(capped_query_schema()),
                true,
            ),
            tool_def(
                "implementations",
                &format!(
                    "Find implementations of a trait/interface/base class (Rust traits, TypeScript interfaces, Python/JavaScript bases, explicit Go assertions; structural Go matches are not inferred). Optional module narrows same-named traits.{trust}"
                ),
                with_preview(capped_query_schema()),
                true,
            ),
            tool_def(
                "dependencies",
                &format!(
                    "Find modules/files a module or symbol depends on. Pass a module path (e.g. crate::mcp), directory, file path, symbol, qualified symbol (e.g. crate::mcp::serve), or member (e.g. Type.member).{trust}"
                ),
                capped_target_schema(),
                true,
            ),
            tool_def(
                "impact",
                &format!(
                    "Find symbols transitively impacted by changing a name. Candidate blast radius only — check confidence/notes before editing.{trust}"
                ),
                with_preview(capped_query_schema()),
                true,
            ),
            tool_def(
                "outline",
                &format!(
                    "List symbols defined in a file, module, or directory, in source order. Pass the indexed path (e.g. src/auth.ts), a module path (whole subtree), or a directory; absolute paths work too.{trust}"
                ),
                with_preview(capped_path_schema()),
                true,
            ),
            tool_def(
                "search",
                &format!(
                    "Search symbol names by substring (case-insensitive) when the exact name is unknown. Exact matches rank first; `Type.member` resolves the member exactly.{trust}"
                ),
                with_preview(search_schema()),
                true,
            ),
            tool_def(
                "unused",
                &format!(
                    "Find functions/methods with no recorded references (candidate dead code). Omit path to sweep the whole project, or scope to a file, module, or directory. Candidates only — verify before deleting.{trust}"
                ),
                with_preview(optional_path_schema()),
                true,
            ),
            tool_def(
                "dependents",
                &format!(
                    "Find modules that depend on a module, file, or symbol (reverse dependencies). Pass a module path (e.g. crate::mcp), directory, file path, symbol, qualified symbol (e.g. crate::mcp::serve), or member (e.g. Type.member).{trust}"
                ),
                capped_target_schema(),
                true,
            ),
            tool_def(
                "index",
                "Index a repository path into its own .keel/index.db (writes the local index; paths outside the server project are safe).",
                json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Filesystem path of the repository to index"
                        }
                    },
                    "required": ["path"]
                }),
                false,
            ),
        ]
    })
}

fn tool_def(name: &str, description: &str, input_schema: Value, read_only: bool) -> Value {
    let mut tool = json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema,
    });
    if read_only {
        tool["annotations"] = json!({
            "readOnlyHint": true,
            "destructiveHint": false,
            "idempotentHint": true,
        });
    }
    tool
}

fn path_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "Indexed file path (e.g. src/auth.ts; absolute paths work too), module path (e.g. crate::mcp, whole subtree), or directory (e.g. src/graph)"
            }
        },
        "required": ["path"]
    })
}

/// `path_schema` plus an optional cap: subtree outlines can run hot.
fn capped_path_schema() -> Value {
    let mut schema = path_schema();
    schema["properties"]["limit"] = json!({
        "type": "integer",
        "description": "Maximum matches to return (1-100000, default 500). Capped responses carry a note with the true total."
    });
    schema
}

/// `capped_path_schema` with `path` optional (`unused` sweeps the whole
/// project when no target is given), plus the transitive dead-code pass.
fn optional_path_schema() -> Value {
    let mut schema = capped_path_schema();
    schema["required"] = json!([]);
    schema["properties"]["path"]["description"] = json!(
        "Indexed file path (e.g. src/auth.ts; absolute paths work too), module path (whole subtree), or directory. Omit to sweep the whole project."
    );
    schema["properties"]["transitive"] = json!({
        "type": "boolean",
        "description": "Also flag functions referenced only from other candidate-dead functions."
    });
    schema
}

/// Target schema for `dependencies`/`dependents`: unlike plain symbol
/// queries, these also accept module paths and file paths.
fn target_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "Module path (e.g. crate::mcp), directory (e.g. src/graph), indexed file path (e.g. src/auth.ts), symbol, or qualified symbol (e.g. crate::mcp::serve). Parent modules and directories cover their whole subtree."
            }
        },
        "required": ["name"]
    })
}

fn search_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "pattern": {
                "type": "string",
                "description": "Substring to match against symbol names (case-insensitive)"
            },
            "limit": {
                "type": "integer",
                "description": "Maximum matches to return (1-200, default 50)"
            }
        },
        "required": ["pattern"]
    })
}

fn query_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "Symbol name, or qualified name (module::symbol) such as crate::mcp::serve"
            },
            "module": {
                "type": "string",
                "description": "Optional module_path to disambiguate when multiple definitions share the same name (e.g. crate::mcp)"
            }
        },
        "required": ["name"]
    })
}

/// `target_schema` plus an optional cap for hit lists that can run hot
/// (`dependencies`, `dependents`).
fn capped_target_schema() -> Value {
    let mut schema = target_schema();
    schema["properties"]["limit"] = json!({
        "type": "integer",
        "description": "Maximum matches to return (1-100000, default 500). Capped responses carry a note with the true total."
    });
    schema
}

/// `query_schema` plus an optional cap for hit lists that can run hot
/// (`references`, `callers`, `impact`, `implementations`).
fn capped_query_schema() -> Value {
    let mut schema = query_schema();
    schema["properties"]["limit"] = json!({
        "type": "integer",
        "description": "Maximum matches to return (1-100000, default 500). Capped responses carry a note with the true total."
    });
    schema
}

/// Add the `preview` flag to a hit-list tool schema: when true, each hit
/// carries its source line as a `preview` field (truncated at 200 chars).
fn with_preview(mut schema: Value) -> Value {
    schema["properties"]["preview"] = json!({
        "type": "boolean",
        "description": "Include the source line of each hit as a `preview` field (truncated at 200 characters; stale rows carry none)."
    });
    schema
}

fn optional_preview_arg(arguments: &Value) -> bool {
    arguments
        .get("preview")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

fn optional_module_arg(arguments: &Value) -> Option<String> {
    arguments
        .get("module")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

/// Required symbol/`pattern` argument: trimmed, since padding is an agent
/// typo, never part of a name. Paths keep [`require_string_arg`] (raw).
fn require_symbol_arg(arguments: &Value, key: &str) -> Result<String> {
    let value = arguments
        .get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_owned())
        .ok_or_else(|| KeelError::Mcp(format!("missing required argument: {key}")))?;
    if value.is_empty() {
        return Err(KeelError::Mcp(format!(
            "argument `{key}` must not be empty"
        )));
    }
    Ok(value)
}

fn optional_limit_arg(arguments: &Value) -> Option<usize> {
    // No zero-filter: `resolve_limit` clamps 0 → 1, matching `search`.
    arguments
        .get("limit")
        .and_then(|v| v.as_u64())
        .and_then(|n| usize::try_from(n).ok())
}

fn call_tool(
    conn: &mut Connection,
    params: &Value,
    auto_index: bool,
    root: &Path,
) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or_else(|| KeelError::Mcp("tools/call missing name".into()))?;
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

    let is_query = matches!(
        name,
        "definition"
            | "references"
            | "callers"
            | "implementations"
            | "dependencies"
            | "dependents"
            | "impact"
            | "outline"
            | "search"
    );
    if auto_index && is_query {
        let stats = index::index_repository(root, conn)?;
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

    let payload = match name {
        "definition" => {
            let symbol = require_symbol_arg(&arguments, "name")?;
            let module = optional_module_arg(&arguments);
            let limit = optional_limit_arg(&arguments);
            let mut previews = optional_preview_arg(&arguments)
                .then(|| PreviewCache::new(root.to_path_buf()));
            let start = std::time::Instant::now();
            let qr = facade::definition_with_meta_opts(conn, &symbol, module.as_deref())?
                .truncated(crate::graph::query_result::resolve_limit(limit))
                .map_results(|s| {
                    let mut d = SymbolDto::from(&s);
                    if let Some(cache) = previews.as_mut() {
                        d.preview = cache.line(&s.file, s.start_line);
                    }
                    d
                });
            log_tool_query(conn, name, &symbol, module.as_deref(), &qr, start);
            json_text(qr)?
        }
        "references" => {
            let symbol = require_symbol_arg(&arguments, "name")?;
            let module = optional_module_arg(&arguments);
            let limit = optional_limit_arg(&arguments);
            let mut previews = optional_preview_arg(&arguments)
                .then(|| PreviewCache::new(root.to_path_buf()));
            let start = std::time::Instant::now();
            let qr = facade::references_with_meta_opts(conn, &symbol, module.as_deref())?
                .truncated(crate::graph::query_result::resolve_limit(limit))
                .map_results(|r| {
                    let mut d = ReferenceDto::from(&r);
                    if let Some(cache) = previews.as_mut() {
                        d.preview = cache.line(&r.file, r.start_line);
                    }
                    d
                });
            log_tool_query(conn, name, &symbol, module.as_deref(), &qr, start);
            json_text(qr)?
        }
        "callers" => {
            let symbol = require_symbol_arg(&arguments, "name")?;
            let module = optional_module_arg(&arguments);
            let limit = optional_limit_arg(&arguments);
            let mut previews = optional_preview_arg(&arguments)
                .then(|| PreviewCache::new(root.to_path_buf()));
            let start = std::time::Instant::now();
            let qr = facade::callers_with_meta_opts(conn, &symbol, module.as_deref())?
                .truncated(crate::graph::query_result::resolve_limit(limit))
                .map_results(|r| {
                    let mut d = ReferenceDto::from(&r);
                    if let Some(cache) = previews.as_mut() {
                        d.preview = cache.line(&r.file, r.start_line);
                    }
                    d
                });
            log_tool_query(conn, name, &symbol, module.as_deref(), &qr, start);
            json_text(qr)?
        }
        "implementations" => {
            let symbol = require_symbol_arg(&arguments, "name")?;
            let module = optional_module_arg(&arguments);
            let limit = optional_limit_arg(&arguments);
            let mut previews = optional_preview_arg(&arguments)
                .then(|| PreviewCache::new(root.to_path_buf()));
            let start = std::time::Instant::now();
            let qr = facade::implementations_with_meta_opts(conn, &symbol, module.as_deref())?
                .truncated(crate::graph::query_result::resolve_limit(limit))
                .map_results(|i| {
                    let mut d = ImplDto::from(&i);
                    if let Some(cache) = previews.as_mut() {
                        d.preview = cache.line(&i.file, i.start_line);
                    }
                    d
                });
            log_tool_query(conn, name, &symbol, module.as_deref(), &qr, start);
            json_text(qr)?
        }
        "dependencies" => {
            let symbol = require_symbol_arg(&arguments, "name")?;
            let limit = optional_limit_arg(&arguments);
            let start = std::time::Instant::now();
            let qr = facade::dependencies_with_meta(conn, &symbol)?
                .truncated(crate::graph::query_result::resolve_limit(limit))
                .map_results(|d| DependencyDto::from(&d));
            log_tool_query(conn, name, &symbol, None, &qr, start);
            json_text(qr)?
        }
        "dependents" => {
            let symbol = require_symbol_arg(&arguments, "name")?;
            let limit = optional_limit_arg(&arguments);
            let start = std::time::Instant::now();
            let qr = facade::dependents_with_meta(conn, &symbol)?
                .truncated(crate::graph::query_result::resolve_limit(limit))
                .map_results(|d| DependencyDto::from(&d));
            log_tool_query(conn, name, &symbol, None, &qr, start);
            json_text(qr)?
        }
        "impact" => {
            let symbol = require_symbol_arg(&arguments, "name")?;
            let module = optional_module_arg(&arguments);
            let limit = optional_limit_arg(&arguments);
            let mut previews = optional_preview_arg(&arguments)
                .then(|| PreviewCache::new(root.to_path_buf()));
            let start = std::time::Instant::now();
            let qr = facade::impact_with_meta_opts(conn, &symbol, module.as_deref())?
                .truncated(crate::graph::query_result::resolve_limit(limit))
                .map_results(|s| {
                    let mut d = SymbolDto::from(&s);
                    if let Some(cache) = previews.as_mut() {
                        d.preview = cache.line(&s.file, s.start_line);
                    }
                    d
                });
            log_tool_query(conn, name, &symbol, module.as_deref(), &qr, start);
            json_text(qr)?
        }
        "outline" => {
            let path = require_string_arg(&arguments, "path")?;
            let limit = optional_limit_arg(&arguments);
            let mut previews = optional_preview_arg(&arguments)
                .then(|| PreviewCache::new(root.to_path_buf()));
            let start = std::time::Instant::now();
            let qr = facade::outline_with_meta(conn, &path)?
                .truncated(crate::graph::query_result::resolve_limit(limit))
                .map_results(|s| {
                    let mut d = SymbolDto::from(&s);
                    if let Some(cache) = previews.as_mut() {
                        d.preview = cache.line(&s.file, s.start_line);
                    }
                    d
                });
            log_tool_query(conn, name, &path, None, &qr, start);
            json_text(qr)?
        }
        "search" => {
            let pattern = require_symbol_arg(&arguments, "pattern")?;
            let limit = arguments
                .get("limit")
                .and_then(|v| v.as_u64())
                .map(|n| n as usize)
                .unwrap_or(50);
            let mut previews = optional_preview_arg(&arguments)
                .then(|| PreviewCache::new(root.to_path_buf()));
            let start = std::time::Instant::now();
            let qr = facade::search_with_meta(conn, &pattern, limit)?.map_results(|s| {
                let mut d = SymbolDto::from(&s);
                if let Some(cache) = previews.as_mut() {
                    d.preview = cache.line(&s.file, s.start_line);
                }
                d
            });
            log_tool_query(conn, name, &pattern, None, &qr, start);
            json_text(qr)?
        }
        "unused" => {
            let path = arguments
                .get("path")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .filter(|s| !s.is_empty());
            let limit = optional_limit_arg(&arguments);
            let transitive = arguments
                .get("transitive")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let mut previews = optional_preview_arg(&arguments)
                .then(|| PreviewCache::new(root.to_path_buf()));
            let start = std::time::Instant::now();
            let qr = facade::unused_with_meta_opts(conn, path.as_deref(), transitive)?
                .truncated(crate::graph::query_result::resolve_limit(limit))
                .map_results(|s| {
                    let mut d = SymbolDto::from(&s);
                    if let Some(cache) = previews.as_mut() {
                        d.preview = cache.line(&s.file, s.start_line);
                    }
                    d
                });
            log_tool_query(conn, name, path.as_deref().unwrap_or("project"), None, &qr, start);
            json_text(qr)?
        }
        "index" => {
            let path = require_string_arg(&arguments, "path")?;
            // The target's own `<path>/.keel/index.db` — never the server
            // connection: reconciling another root into this DB would wipe
            // this project's rows (and answer with foreign paths).
            // Sources are chained into the message: JSON-RPC errors only
            // carry `Display`, which would otherwise hide the reason.
            let root = Path::new(&path);
            let stats = crate::cli::commands::run_index(root).map_err(|e| {
                let mut msg = format!("cannot index {}: {e}", root.display());
                let mut next = std::error::Error::source(&e);
                while let Some(source) = next {
                    msg.push_str(": ");
                    msg.push_str(&source.to_string());
                    next = std::error::Error::source(source);
                }
                KeelError::Mcp(msg)
            })?;
            json_text(IndexStatsDto::from(&stats))?
        }
        other => {
            return Err(KeelError::Mcp(format!("unknown tool: {other}")));
        }
    };

    Ok(payload)
}

/// Record one MCP tool call for the Insights portal (best-effort, local only).
fn log_tool_query<T>(
    conn: &Connection,
    tool: &str,
    symbol: &str,
    module: Option<&str>,
    qr: &crate::graph::query_result::QueryResult<T>,
    start: std::time::Instant,
) {
    let summary = crate::usage::QuerySummary::from_query_result(qr);
    crate::usage::log_query(
        conn,
        crate::usage::Surface::Mcp,
        tool,
        symbol,
        module,
        &summary,
        start.elapsed().as_millis() as u64,
    );
}

fn require_string_arg(arguments: &Value, key: &str) -> Result<String> {
    let value = arguments
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .ok_or_else(|| KeelError::Mcp(format!("missing required argument: {key}")))?;
    // Empty required args are caller bugs, not misses: fail loudly like the
    // CLI (clap exit 2) and HTTP (400) instead of querying for ``.
    if value.is_empty() {
        return Err(KeelError::Mcp(format!(
            "argument `{key}` must not be empty"
        )));
    }
    Ok(value)
}

fn json_text<T: Serialize>(value: T) -> Result<Value> {
    let text =
        serde_json::to_string(&value).map_err(|e| KeelError::Mcp(e.to_string()))?;
    Ok(json!({
        "content": [
            {
                "type": "text",
                "text": text,
            }
        ]
    }))
}

/// Serializable view of [`IndexStats`] for MCP tool results.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct IndexStatsDto {
    indexed: usize,
    skipped: usize,
    removed: usize,
    errors: usize,
    syntax_errors: usize,
}

impl From<&IndexStats> for IndexStatsDto {
    fn from(s: &IndexStats) -> Self {
        Self {
            indexed: s.indexed,
            skipped: s.skipped,
            removed: s.removed,
            errors: s.errors,
            syntax_errors: s.syntax_errors,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use serde_json::json;
    use std::io::Cursor;

    #[test]
    fn encode_message_writes_content_length_frame() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let framed = encode_message(body);
        let text = String::from_utf8(framed.clone()).unwrap();
        assert!(
            text.starts_with(&format!("Content-Length: {}\r\n", body.len())),
            "frame must start with Content-Length header"
        );
        let sep = text.find("\r\n\r\n").expect("header/body separator");
        assert_eq!(&framed[sep + 4..], body);
    }

    #[test]
    fn encode_message_writes_ndjson_frame() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let framed = encode_message_with(WireFormat::Ndjson, body);
        assert_eq!(&framed[..body.len()], body);
        assert_eq!(*framed.last().unwrap(), b'\n');
    }

    #[test]
    fn read_message_decodes_content_length_frame() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let framed = encode_message(body);
        let mut cursor = Cursor::new(framed);
        let mut format = None;
        let decoded = read_message(&mut cursor, &mut format).expect("decode framed message");
        assert_eq!(format, Some(WireFormat::ContentLength));
        assert_eq!(decoded, body);
    }

    #[test]
    fn read_message_decodes_ndjson_frame() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let mut input = body.to_vec();
        input.push(b'\n');
        let mut cursor = Cursor::new(input);
        let mut format = None;
        let decoded = read_message(&mut cursor, &mut format).expect("decode ndjson");
        assert_eq!(format, Some(WireFormat::Ndjson));
        assert_eq!(decoded, body);
    }

    #[test]
    fn handle_initialize_returns_server_capabilities() {
        let mut conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "test", "version": "0"}
            }
        });
        let resp = handle_message(&mut conn, &msg)
            .expect("initialize")
            .expect("initialize must return a response");
        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], 1);
        assert!(resp["result"]["capabilities"]["tools"].is_object());
        assert_eq!(resp["result"]["serverInfo"]["name"], "keel");
        assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
    }

    #[test]
    fn initialize_echoes_supported_newer_protocol_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "cursor", "version": "1"}
            }
        });
        let resp = handle_message(&mut conn, &msg)
            .expect("initialize")
            .expect("initialize must return a response");
        assert_eq!(resp["result"]["protocolVersion"], "2025-06-18");
    }

    #[test]
    fn initialize_echoes_cursor_2025_11_25_protocol() {
        let mut conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": {"name": "cursor-vscode", "version": "1.0.0"}
            }
        });
        let resp = handle_message(&mut conn, &msg)
            .expect("initialize")
            .expect("initialize must return a response");
        assert_eq!(resp["result"]["protocolVersion"], "2025-11-25");
    }

    #[test]
    fn handle_tools_list_includes_code_intelligence_tools() {
        let mut conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list"
        });
        let resp = handle_message(&mut conn, &msg)
            .expect("tools/list")
            .expect("tools/list must return a response");
        let tools = resp["result"]["tools"]
            .as_array()
            .expect("tools array");
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|t| t["name"].as_str())
            .collect();
        for expected in [
            "definition",
            "references",
            "callers",
            "implementations",
            "dependencies",
            "dependents",
            "impact",
            "outline",
            "search",
            "index",
        ] {
            assert!(
                names.contains(&expected),
                "missing tool {expected}; have {names:?}"
            );
        }
        let definition = tools
            .iter()
            .find(|t| t["name"] == "definition")
            .expect("definition tool");
        let desc = definition["description"].as_str().unwrap_or("");
        assert!(
            desc.contains("confidence"),
            "definition description should teach trust envelope, got: {desc}"
        );
        assert!(
            definition["inputSchema"]["properties"]["module"].is_object(),
            "definition should accept optional module"
        );
        assert_eq!(
            definition["annotations"]["readOnlyHint"],
            true,
            "definition should be readOnly for agent approval UX"
        );
        assert!(
            desc.contains("1=strongest"),
            "trust envelope should teach tier ordering, got: {desc}"
        );
        for tool_name in ["dependencies", "dependents"] {
            let tool = tools
                .iter()
                .find(|t| t["name"] == tool_name)
                .unwrap_or_else(|| panic!("{tool_name} tool"));
            let arg = tool["inputSchema"]["properties"]["name"]["description"]
                .as_str()
                .unwrap_or("");
            assert!(
                arg.contains("file path"),
                "{tool_name} arg should document file-path targets, got: {arg}"
            );
        }
        for tool_name in [
            "definition",
            "references",
            "callers",
            "impact",
            "dependencies",
            "dependents",
            "implementations",
            "outline",
        ] {
            let tool = tools
                .iter()
                .find(|t| t["name"] == tool_name)
                .unwrap_or_else(|| panic!("{tool_name} tool"));
            assert!(
                tool["inputSchema"]["properties"].get("limit").is_some(),
                "{tool_name} should accept an optional limit"
            );
        }
    }

    #[test]
    fn require_string_arg_rejects_missing_and_empty() {
        let missing = require_string_arg(&json!({}), "name").unwrap_err();
        assert!(
            missing.to_string().contains("missing required argument"),
            "got: {missing}"
        );
        let empty = require_string_arg(&json!({"name": ""}), "name").unwrap_err();
        assert!(
            empty.to_string().contains("must not be empty"),
            "empty required args must fail loudly, got: {empty}"
        );
        assert_eq!(
            require_string_arg(&json!({"name": "serve"}), "name").unwrap(),
            "serve"
        );
    }

    #[test]
    fn require_symbol_arg_trims_padding_but_rejects_blank() {
        assert_eq!(
            require_symbol_arg(&json!({"name": "  serve "}), "name").unwrap(),
            "serve"
        );
        assert_eq!(
            require_symbol_arg(&json!({"pattern": "\torder\n"}), "pattern").unwrap(),
            "order"
        );
        let blank = require_symbol_arg(&json!({"name": "   "}), "name").unwrap_err();
        assert!(
            blank.to_string().contains("must not be empty"),
            "got: {blank}"
        );
        // Paths stay raw: trailing whitespace may be significant.
        assert_eq!(
            require_string_arg(&json!({"path": "dir/ "}), "path").unwrap(),
            "dir/ "
        );
        // A padded module still filters (whitespace-only means no filter).
        assert_eq!(
            optional_module_arg(&json!({"module": " crate::mcp "})),
            Some("crate::mcp".to_string())
        );
        assert_eq!(optional_module_arg(&json!({"module": "  "})), None);
    }

    #[test]
    fn handle_resources_and_prompts_list_return_empty() {
        let mut conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        for (method, key) in [
            ("resources/list", "resources"),
            ("resources/templates/list", "resourceTemplates"),
            ("prompts/list", "prompts"),
        ] {
            let msg = json!({
                "jsonrpc": "2.0",
                "id": 3,
                "method": method
            });
            let resp = handle_message(&mut conn, &msg)
                .unwrap_or_else(|e| panic!("{method}: {e}"))
                .unwrap_or_else(|| panic!("{method} must return a response"));
            let arr = resp["result"][key]
                .as_array()
                .unwrap_or_else(|| panic!("{method} missing {key}"));
            assert!(arr.is_empty(), "{method} should be empty");
        }
    }

    #[test]
    fn handle_initialized_notification_returns_none() {
        let mut conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        let msg = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        });
        let resp = handle_message(&mut conn, &msg).expect("notification");
        assert!(resp.is_none());
    }

    #[test]
    fn tools_call_index_rejects_missing_dirs() {
        let mut conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        let missing =
            std::env::temp_dir().join("keel-mcp-index-missing-9f3b2a1c");
        let _ = std::fs::remove_dir_all(&missing);
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": {"name": "index", "arguments": {"path": missing.to_string_lossy()}}
        });
        let resp = handle_message(&mut conn, &msg)
            .expect("index call")
            .expect("index must return a response");
        let message = resp["error"]["message"].as_str().unwrap_or("");
        assert!(message.contains("no such directory"), "got: {resp}");
        assert!(!missing.exists(), "typo'd path must not be created");
    }

    #[test]
    fn tools_call_index_writes_target_db_not_server_db() {
        use crate::db::queries;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join("solo.rs"), "fn solo() {}\n").unwrap();

        // Server connection serves some other project (in-memory here).
        let mut conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 12,
            "method": "tools/call",
            "params": {"name": "index", "arguments": {"path": root.to_string_lossy()}}
        });
        let resp = handle_message(&mut conn, &msg)
            .expect("index call")
            .expect("index must return a response");
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .expect("text payload");
        let payload: Value = serde_json::from_str(text).unwrap();
        assert_eq!(payload["indexed"], 1);

        // Target got its own index…
        let target_db = root.join(".keel").join("index.db");
        assert!(target_db.is_file());
        let target_conn = Connection::open(&target_db).unwrap();
        assert_eq!(queries::find_definition(&target_conn, "solo").unwrap().len(), 1);
        // …while the server connection stayed empty.
        assert!(queries::find_definition(&conn, "solo").unwrap().is_empty());
    }

    #[test]
    fn tools_call_outline_lists_file_symbols_in_order() {
        use crate::db::queries;
        use crate::graph::types::{FileNode, Symbol, SymbolKind};
        use std::path::PathBuf;

        let mut conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        let file_id = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("src/lib.rs"),
                content_hash: "h".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            &conn,
            file_id,
            &[
                Symbol {
                    name: "B".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: 9,
                    start_col: 1,
                    module_path: "crate".into(),
                    container: String::new(),
                },
                Symbol {
                    name: "A".into(),
                    kind: SymbolKind::Struct,
                    file: PathBuf::new(),
                    start_line: 1,
                    start_col: 1,
                    module_path: "crate".into(),
                    container: String::new(),
                },
            ],
        )
        .unwrap();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "tools/call",
            "params": {"name": "outline", "arguments": {"path": "src/lib.rs"}}
        });
        let resp = handle_message(&mut conn, &msg)
            .expect("outline call")
            .expect("outline must return a response");
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .expect("text payload");
        let payload: Value = serde_json::from_str(text).unwrap();
        let names: Vec<&str> = payload["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["A", "B"]);
        assert_eq!(payload["confidence"], "high");
    }

    #[test]
    fn tools_call_outline_honors_limit() {
        use crate::db::queries;
        use crate::graph::types::{FileNode, Symbol, SymbolKind};
        use std::path::PathBuf;

        let mut conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        let file_id = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("src/lib.rs"),
                content_hash: "h".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            &conn,
            file_id,
            &[
                Symbol {
                    name: "A".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: 1,
                    start_col: 1,
                    module_path: "crate".into(),
                    container: String::new(),
                },
                Symbol {
                    name: "B".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: 9,
                    start_col: 1,
                    module_path: "crate".into(),
                    container: String::new(),
                },
            ],
        )
        .unwrap();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "tools/call",
            "params": {"name": "outline", "arguments": {"path": "src/lib.rs", "limit": 1}}
        });
        let resp = handle_message(&mut conn, &msg)
            .expect("outline call")
            .expect("outline must return a response");
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .expect("text payload");
        let payload: Value = serde_json::from_str(text).unwrap();
        assert_eq!(
            payload["results"].as_array().unwrap().len(),
            1,
            "limit 1 must cap two symbols, got: {payload}"
        );
        assert_eq!(payload["results"][0]["name"], "A");
        let notes = payload["notes"].as_array().unwrap();
        assert!(
            notes.iter().any(|n| n
                .as_str()
                .unwrap_or("")
                .contains("Showing first 1 of 2 matches")),
            "capped response must carry the true total, got: {payload}"
        );
    }

    #[test]
    fn tools_call_dependents_lists_importing_modules() {
        use crate::db::queries;
        use crate::graph::types::{FileNode, Import, Symbol, SymbolKind};
        use std::path::PathBuf;

        let mut conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        let b = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("src/b.rs"),
                content_hash: "hb".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            &conn,
            b,
            &[Symbol {
                name: "f".into(),
                kind: SymbolKind::Function,
                file: PathBuf::new(),
                start_line: 1,
                start_col: 1,
                module_path: "crate::b".into(),
                container: String::new(),
            }],
        )
        .unwrap();
        let a = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("src/a.rs"),
                content_hash: "ha".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            &conn,
            a,
            &[Symbol {
                name: "g".into(),
                kind: SymbolKind::Function,
                file: PathBuf::new(),
                start_line: 1,
                start_col: 1,
                module_path: "crate::a".into(),
                container: String::new(),
            }],
        )
        .unwrap();
        queries::insert_imports(
            &conn,
            a,
            &[Import {
                module_path: "crate::b".into(),
                alias: None,
                file: PathBuf::new(),
            }],
        )
        .unwrap();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/call",
            "params": {"name": "dependents", "arguments": {"name": "crate::b"}}
        });
        let resp = handle_message(&mut conn, &msg)
            .expect("dependents call")
            .expect("dependents must return a response");
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .expect("text payload");
        let payload: Value = serde_json::from_str(text).unwrap();
        let modules: Vec<&str> = payload["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["module_path"].as_str().unwrap())
            .collect();
        assert_eq!(modules, vec!["crate::a"]);
        assert_eq!(payload["confidence"], "high");
    }

    #[test]
    fn tools_call_dependents_honors_limit() {
        use crate::db::queries;
        use crate::graph::types::{FileNode, Import, Symbol, SymbolKind};
        use std::path::PathBuf;

        let mut conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        let b = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("src/b.rs"),
                content_hash: "hb".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            &conn,
            b,
            &[Symbol {
                name: "f".into(),
                kind: SymbolKind::Function,
                file: PathBuf::new(),
                start_line: 1,
                start_col: 1,
                module_path: "crate::b".into(),
                container: String::new(),
            }],
        )
        .unwrap();
        for (path, module) in [
            ("src/a.rs", "crate::a"),
            ("src/c.rs", "crate::c"),
        ] {
            let id = queries::insert_file(
                &conn,
                &FileNode {
                    path: PathBuf::from(path),
                    content_hash: "h".into(),
                },
            )
            .unwrap();
            queries::insert_symbols(
                &conn,
                id,
                &[Symbol {
                    name: "g".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: 1,
                    start_col: 1,
                    module_path: module.into(),
                    container: String::new(),
                }],
            )
            .unwrap();
            queries::insert_imports(
                &conn,
                id,
                &[Import {
                    module_path: "crate::b".into(),
                    alias: None,
                    file: PathBuf::new(),
                }],
            )
            .unwrap();
        }
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/call",
            "params": {"name": "dependents", "arguments": {"name": "crate::b", "limit": 1}}
        });
        let resp = handle_message(&mut conn, &msg)
            .expect("dependents call")
            .expect("dependents must return a response");
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .expect("text payload");
        let payload: Value = serde_json::from_str(text).unwrap();
        assert_eq!(
            payload["results"].as_array().unwrap().len(),
            1,
            "limit 1 must cap two dependents, got: {payload}"
        );
        let notes = payload["notes"].as_array().unwrap();
        assert!(
            notes.iter().any(|n| n
                .as_str()
                .unwrap_or("")
                .contains("Showing first 1 of 2 matches")),
            "capped response must carry the true total, got: {payload}"
        );
    }

    #[test]
    fn tools_call_search_ranks_exact_first_and_honors_limit() {
        use crate::db::queries;
        use crate::graph::types::{FileNode, Symbol, SymbolKind};
        use std::path::PathBuf;

        let mut conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        let file_id = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("src/lib.rs"),
                content_hash: "h".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            &conn,
            file_id,
            &[
                Symbol {
                    name: "reorder_buffer".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: 9,
                    start_col: 1,
                    module_path: "crate".into(),
                    container: String::new(),
                },
                Symbol {
                    name: "order".into(),
                    kind: SymbolKind::Struct,
                    file: PathBuf::new(),
                    start_line: 1,
                    start_col: 1,
                    module_path: "crate".into(),
                    container: String::new(),
                },
            ],
        )
        .unwrap();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 8,
            "method": "tools/call",
            "params": {"name": "search", "arguments": {"pattern": "order"}}
        });
        let resp = handle_message(&mut conn, &msg)
            .expect("search call")
            .expect("search must return a response");
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .expect("text payload");
        let payload: Value = serde_json::from_str(text).unwrap();
        let names: Vec<&str> = payload["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["order", "reorder_buffer"]);

        let msg = json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "tools/call",
            "params": {"name": "search", "arguments": {"pattern": "order", "limit": 1}}
        });
        let resp = handle_message(&mut conn, &msg)
            .expect("search call")
            .expect("search must return a response");
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .expect("text payload");
        let payload: Value = serde_json::from_str(text).unwrap();
        assert_eq!(payload["results"].as_array().unwrap().len(), 1);
        assert!(
            payload["notes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|n| n.as_str().unwrap_or("").contains("first 1 matches")),
            "got {:?}",
            payload["notes"]
        );
    }

    #[test]
    fn tools_list_advertises_preview_on_hit_tools_only() {
        let mut conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 40,
            "method": "tools/list"
        });
        let resp = handle_message(&mut conn, &msg)
            .expect("tools/list")
            .expect("tools/list must return a response");
        let tools = resp["result"]["tools"].as_array().expect("tools array");
        let has_preview = |tool_name: &str| -> bool {
            tools
                .iter()
                .find(|t| t["name"] == tool_name)
                .unwrap_or_else(|| panic!("{tool_name} tool"))["inputSchema"]["properties"]
                .get("preview")
                .is_some()
        };
        for tool_name in [
            "definition",
            "references",
            "callers",
            "implementations",
            "impact",
            "outline",
            "search",
            "unused",
        ] {
            assert!(has_preview(tool_name), "{tool_name} should accept preview");
        }
        for tool_name in ["dependencies", "dependents", "index"] {
            assert!(
                !has_preview(tool_name),
                "{tool_name} must not advertise preview"
            );
        }
    }

    #[test]
    fn tools_call_definition_preview_reads_project_sources() {
        use crate::db::queries;
        use crate::graph::types::{FileNode, Symbol, SymbolKind};
        use std::path::PathBuf;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/lib.rs"), "pub struct Solo;\n").unwrap();

        let mut conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        let file_id = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("src/lib.rs"),
                content_hash: "h".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            &conn,
            file_id,
            &[Symbol {
                name: "Solo".into(),
                kind: SymbolKind::Struct,
                file: PathBuf::new(),
                start_line: 1,
                start_col: 12,
                module_path: "crate".into(),
                container: String::new(),
            }],
        )
        .unwrap();

        let msg = json!({
            "jsonrpc": "2.0",
            "id": 41,
            "method": "tools/call",
            "params": {"name": "definition", "arguments": {"name": "Solo", "preview": true}}
        });
        let resp = handle_message_with(&mut conn, &msg, false, root)
            .expect("definition call")
            .expect("definition must return a response");
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .expect("text payload");
        let payload: Value = serde_json::from_str(text).unwrap();
        assert_eq!(
            payload["results"][0]["preview"].as_str(),
            Some("pub struct Solo;"),
            "got: {payload}"
        );

        // Without the flag the field stays out.
        let msg = json!({
            "jsonrpc": "2.0",
            "id": 42,
            "method": "tools/call",
            "params": {"name": "definition", "arguments": {"name": "Solo"}}
        });
        let resp = handle_message_with(&mut conn, &msg, false, root)
            .expect("definition call")
            .expect("definition must return a response");
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .expect("text payload");
        let payload: Value = serde_json::from_str(text).unwrap();
        assert!(
            payload["results"][0].get("preview").is_none(),
            "got: {payload}"
        );
    }
}
