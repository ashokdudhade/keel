//! MCP stdio server: Content-Length framed JSON-RPC 2.0 over stdin/stdout.
//!
//! Logs must go to stderr only — stdout is reserved for framed JSON-RPC.

use crate::api::{DependencyDto, ImplDto, ReferenceDto, SymbolDto};
use crate::db::{queries, schema};
use crate::error::{Result, SecondBrainError};
use crate::graph::deps;
use crate::graph::impact;
use crate::graph::resolve;
use crate::graph::types::Symbol;
use crate::index::{self, IndexStats};
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{json, Value};
use std::io::{self, BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "secondbrain";

/// Encode a JSON-RPC body as a Content-Length framed MCP message.
pub fn encode_message(body: &[u8]) -> Vec<u8> {
    let header = format!("Content-Length: {}\r\n\r\n", body.len());
    let mut out = Vec::with_capacity(header.len() + body.len());
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(body);
    out
}

/// Read one Content-Length framed MCP message from `reader`.
///
/// Callers should reuse a single [`BufRead`] across the session so buffered
/// bytes from one frame are not dropped before the next.
pub fn read_message(reader: &mut impl BufRead) -> Result<Vec<u8>> {
    let mut content_length: Option<usize> = None;

    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .map_err(|source| SecondBrainError::Io {
                path: PathBuf::from("stdin"),
                source,
            })?;
        if n == 0 {
            return Err(SecondBrainError::Mcp("unexpected EOF while reading headers".into()));
        }
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed.is_empty() {
            break;
        }
        if let Some(rest) = trimmed.strip_prefix("Content-Length:") {
            let len = rest.trim().parse::<usize>().map_err(|_| {
                SecondBrainError::Mcp(format!("invalid Content-Length: {rest:?}"))
            })?;
            content_length = Some(len);
        }
        // Other headers (e.g. Content-Type) are ignored.
    }

    let len = content_length
        .ok_or_else(|| SecondBrainError::Mcp("missing Content-Length header".into()))?;
    let mut body = vec![0u8; len];
    reader
        .read_exact(&mut body)
        .map_err(|source| SecondBrainError::Io {
            path: PathBuf::from("stdin"),
            source,
        })?;
    Ok(body)
}

/// Dispatch a single JSON-RPC request/notification.
///
/// Returns `Ok(None)` for notifications that need no response.
pub fn handle_message(conn: &mut Connection, msg: &Value) -> Result<Option<Value>> {
    let method = msg
        .get("method")
        .and_then(|m| m.as_str())
        .ok_or_else(|| SecondBrainError::Mcp("missing method".into()))?;
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
        "initialize" => Ok(initialize_result()),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools_list_result()),
        "tools/call" => {
            let params = msg.get("params").cloned().unwrap_or(json!({}));
            call_tool(conn, &params)
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
/// Ensures the database directory/schema exist, then blocks on the stdio loop.
pub fn serve(db_path: &Path) -> Result<()> {
    if let Some(parent) = db_path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|source| SecondBrainError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
    }
    let mut conn = Connection::open(db_path)?;
    schema::initialize(&conn)?;

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    let mut reader = BufReader::new(stdin.lock());

    loop {
        let body = match read_message(&mut reader) {
            Ok(b) => b,
            Err(SecondBrainError::Io { source, .. })
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

        let msg: Value = match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("mcp: invalid JSON: {e}");
                let err = json_rpc_error(Value::Null, -32700, format!("Parse error: {e}"));
                write_response(&mut stdout, &err)?;
                continue;
            }
        };

        match handle_message(&mut conn, &msg) {
            Ok(Some(response)) => write_response(&mut stdout, &response)?,
            Ok(None) => {}
            Err(e) => {
                eprintln!("mcp: handler error: {e}");
                let id = msg.get("id").cloned().unwrap_or(Value::Null);
                let err = json_rpc_error(id, -32603, e.to_string());
                write_response(&mut stdout, &err)?;
            }
        }
    }
    Ok(())
}

fn write_response(stdout: &mut impl Write, response: &Value) -> Result<()> {
    let body =
        serde_json::to_vec(response).map_err(|e| SecondBrainError::Mcp(e.to_string()))?;
    let framed = encode_message(&body);
    stdout
        .write_all(&framed)
        .map_err(|source| SecondBrainError::Io {
            path: PathBuf::from("stdout"),
            source,
        })?;
    stdout.flush().map_err(|source| SecondBrainError::Io {
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

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
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
                "Index a repository path into the SecondBrain database.",
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

fn call_tool(conn: &mut Connection, params: &Value) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or_else(|| SecondBrainError::Mcp("tools/call missing name".into()))?;
    let arguments = params.get("arguments").cloned().unwrap_or(json!({}));

    let payload = match name {
        "definition" => {
            let symbol = require_string_arg(&arguments, "name")?;
            let defs = queries::find_definition(conn, &symbol)?;
            json_text(defs.iter().map(SymbolDto::from).collect::<Vec<_>>())?
        }
        "references" => {
            let symbol = require_string_arg(&arguments, "name")?;
            let refs = queries::find_references(conn, &symbol)?;
            json_text(refs.iter().map(ReferenceDto::from).collect::<Vec<_>>())?
        }
        "callers" => {
            let symbol = require_string_arg(&arguments, "name")?;
            let defs = queries::find_definition(conn, &symbol)?;
            let target_module = unique_module(&defs);
            let callers = resolve::find_callers(conn, &symbol, target_module.as_deref())?;
            json_text(callers.iter().map(ReferenceDto::from).collect::<Vec<_>>())?
        }
        "implementations" => {
            let symbol = require_string_arg(&arguments, "name")?;
            let impls = queries::find_implementations(conn, &symbol)?;
            json_text(impls.iter().map(ImplDto::from).collect::<Vec<_>>())?
        }
        "dependencies" => {
            let symbol = require_string_arg(&arguments, "name")?;
            let deps = deps::find_dependencies(conn, &symbol)?;
            json_text(deps.iter().map(DependencyDto::from).collect::<Vec<_>>())?
        }
        "impact" => {
            let symbol = require_string_arg(&arguments, "name")?;
            let impacted = impact::find_impact(conn, &symbol)?;
            json_text(impacted.iter().map(SymbolDto::from).collect::<Vec<_>>())?
        }
        "index" => {
            let path = require_string_arg(&arguments, "path")?;
            let stats = index::index_repository(Path::new(&path), conn)?;
            json_text(IndexStatsDto::from(&stats))?
        }
        other => {
            return Err(SecondBrainError::Mcp(format!("unknown tool: {other}")));
        }
    };

    Ok(payload)
}

fn require_string_arg(arguments: &Value, key: &str) -> Result<String> {
    arguments
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::to_owned)
        .ok_or_else(|| SecondBrainError::Mcp(format!("missing required argument: {key}")))
}

fn json_text<T: Serialize>(value: T) -> Result<Value> {
    let text =
        serde_json::to_string(&value).map_err(|e| SecondBrainError::Mcp(e.to_string()))?;
    Ok(json!({
        "content": [
            {
                "type": "text",
                "text": text,
            }
        ]
    }))
}

fn unique_module(defs: &[Symbol]) -> Option<String> {
    let first = defs.first()?.module_path.clone();
    if defs.iter().all(|d| d.module_path == first) {
        Some(first)
    } else {
        None
    }
}

/// Serializable view of [`IndexStats`] for MCP tool results.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct IndexStatsDto {
    indexed: usize,
    skipped: usize,
    removed: usize,
}

impl From<&IndexStats> for IndexStatsDto {
    fn from(s: &IndexStats) -> Self {
        Self {
            indexed: s.indexed,
            skipped: s.skipped,
            removed: s.removed,
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
        let expected_prefix = format!("Content-Length: {}\r\n\r\n", body.len());
        assert!(
            framed.starts_with(expected_prefix.as_bytes()),
            "frame must start with Content-Length header"
        );
        assert_eq!(&framed[expected_prefix.len()..], body);
    }

    #[test]
    fn read_message_decodes_content_length_frame() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#;
        let framed = encode_message(body);
        let mut cursor = Cursor::new(framed);
        let decoded = read_message(&mut cursor).expect("decode framed message");
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
        assert_eq!(resp["result"]["serverInfo"]["name"], "secondbrain");
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
