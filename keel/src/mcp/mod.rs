//! MCP stdio server: JSON-RPC 2.0 over stdin/stdout.
//!
//! Supports both wire formats used by MCP clients:
//! - **Newline-delimited JSON** (Cursor 2025-11+): one JSON object per line
//! - **Content-Length framing** (older clients / LSP-style)
//!
//! Logs must go to stderr only — stdout is reserved for JSON-RPC.

use crate::api::{DependencyDto, ImplDto, ReferenceDto, SymbolDto};
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
    json!({
        "tools": [
            tool_def(
                "definition",
                "Find definition location(s) for a symbol name.",
                name_schema(),
            ),
            tool_def(
                "references",
                "Find reference sites for a name.",
                name_schema(),
            ),
            tool_def(
                "callers",
                "Find call/use sites of a function or symbol name.",
                name_schema(),
            ),
            tool_def(
                "implementations",
                "Find implementations of a trait name.",
                name_schema(),
            ),
            tool_def(
                "dependencies",
                "Find modules/files that a module or symbol depends on.",
                name_schema(),
            ),
            tool_def(
                "impact",
                "Find symbols transitively impacted by changing a name.",
                name_schema(),
            ),
            tool_def(
                "index",
                "Index a repository path into the Keel database.",
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
            ),
        ]
    })
}

fn tool_def(name: &str, description: &str, input_schema: Value) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": input_schema,
    })
}

fn name_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "Symbol or module name"
            }
        },
        "required": ["name"]
    })
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
        "definition" | "references" | "callers" | "implementations" | "dependencies" | "impact"
    );
    if auto_index && is_query {
        let stats = index::index_repository(root, conn)?;
        if stats.indexed + stats.removed + stats.errors > 0 {
            eprintln!(
                "keel: auto-indexed {} file(s) (skipped {}, removed {}, errors {}).",
                stats.indexed, stats.skipped, stats.removed, stats.errors
            );
        }
    }

    let payload = match name {
        "definition" => {
            let symbol = require_string_arg(&arguments, "name")?;
            let qr = facade::definition_with_meta(conn, &symbol)?
                .map_results(|s| SymbolDto::from(&s));
            json_text(qr)?
        }
        "references" => {
            let symbol = require_string_arg(&arguments, "name")?;
            let qr = facade::references_with_meta(conn, &symbol)?
                .map_results(|r| ReferenceDto::from(&r));
            json_text(qr)?
        }
        "callers" => {
            let symbol = require_string_arg(&arguments, "name")?;
            let qr = facade::callers_with_meta(conn, &symbol)?
                .map_results(|r| ReferenceDto::from(&r));
            json_text(qr)?
        }
        "implementations" => {
            let symbol = require_string_arg(&arguments, "name")?;
            let qr = facade::implementations_with_meta(conn, &symbol)?
                .map_results(|i| ImplDto::from(&i));
            json_text(qr)?
        }
        "dependencies" => {
            let symbol = require_string_arg(&arguments, "name")?;
            let qr = facade::dependencies_with_meta(conn, &symbol)?
                .map_results(|d| DependencyDto::from(&d));
            json_text(qr)?
        }
        "impact" => {
            let symbol = require_string_arg(&arguments, "name")?;
            let qr = facade::impact_with_meta(conn, &symbol)?
                .map_results(|s| SymbolDto::from(&s));
            json_text(qr)?
        }
        "index" => {
            let path = require_string_arg(&arguments, "path")?;
            let stats = index::index_repository(Path::new(&path), conn)?;
            json_text(IndexStatsDto::from(&stats))?
        }
        other => {
            return Err(KeelError::Mcp(format!("unknown tool: {other}")));
        }
    };

    Ok(payload)
}

fn require_string_arg(arguments: &Value, key: &str) -> Result<String> {
    arguments
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .ok_or_else(|| KeelError::Mcp(format!("missing required argument: {key}")))
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
}

impl From<&IndexStats> for IndexStatsDto {
    fn from(s: &IndexStats) -> Self {
        Self {
            indexed: s.indexed,
            skipped: s.skipped,
            removed: s.removed,
            errors: s.errors,
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
            "impact",
            "index",
        ] {
            assert!(
                names.contains(&expected),
                "missing tool {expected}; have {names:?}"
            );
        }
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
}
