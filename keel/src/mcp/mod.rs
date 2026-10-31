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
use crate::context;
use crate::explain;
use crate::facade;
use crate::index::{self, IndexStats};
use crate::knowledge::{self, KnowledgeRecord};
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

/// Full `tools/list` payload. `pub(crate)` so the drift checker can
/// resolve doc citations of tool names against the live list instead of
/// a hardcoded copy that would rot.
pub(crate) fn tools_list_result() -> Value {
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
                "knowledge_search",
                "Search source-linked knowledge records (architecture facts, decisions, explanations, patterns, constraints) by substring over id, title, and content. Each hit carries the full record (provenance in `origin`, freshness in `status`, source pins in `evidence`) plus a live `fresh` verdict: `fresh: false` means the pinned code verifiably drifted — re-verify before relying on the claim. `supported` + `fresh` is the strongest signal; `stale` or `unverified` records are leads, not facts.",
                json!({
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Substring to match against record id, title, and content (case-insensitive)"
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum matches to return (1-200, default 20). Capped responses carry a note with the true total."
                        }
                    },
                    "required": ["query"]
                }),
                true,
            ),
            tool_def(
                "knowledge_get",
                "Fetch one knowledge record by exact id, with its provenance (`origin`), freshness (`status`), source `evidence`, and a live `fresh` verdict. A `null` record with a note means unknown id — never invent record contents. `supported` + `fresh` is the strongest signal; `stale` means the pinned code changed since verification.",
                json!({
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "description": "Exact knowledge record id (e.g. auth.jwt.validation)"
                        }
                    },
                    "required": ["id"]
                }),
                true,
            ),
            tool_def(
                "knowledge_status",
                "Report stored status plus the live freshness verdict per knowledge record. Omit `id` for all records, or pass one id for a single verdict. `drifted` means pinned evidence verifiably changed; `unverifiable` means entries lack hash pins and cannot be judged. Informational only — use `knowledge_validate` to re-check and re-pin one record.",
                json!({
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "description": "Optional knowledge record id; omit to check all records"
                        }
                    },
                    "required": []
                }),
                true,
            ),
            tool_def(
                "knowledge_add",
                "Create or update a knowledge record with source evidence (WRITES the project index). Provide the full record object: `id`, `type` (architecture_fact|decision|explanation|pattern|constraint), `title`, `content` (the claim), `origin` (human|agent_generated|inferred|deterministic), and `evidence` ([{source_path, symbol?, source_hash?, commit_hash?}] with indexed-relative file paths). New records should start `unverified` and earn `supported` via `knowledge_validate`; human-authored records reject non-human overwrites. Timestamps are stamped by the store.",
                json!({
                    "type": "object",
                    "properties": {
                        "record": {
                            "type": "object",
                            "description": "Full knowledge record (id, type, title, content, origin, status, evidence[])"
                        }
                    },
                    "required": ["record"]
                }),
                false,
            ),
            tool_def(
                "knowledge_validate",
                "Re-check one knowledge record against the current code (WRITES pins and status): when its evidence anchors (files indexed, named symbols defined) resolve, refreshes hash pins to current revisions and promotes to `supported`; else marks `stale` with reasons. Promotion is your explicit act — only validate claims you have actually re-verified.",
                json!({
                    "type": "object",
                    "properties": {
                        "id": {
                            "type": "string",
                            "description": "Knowledge record id to validate"
                        }
                    },
                    "required": ["id"]
                }),
                false,
            ),
            tool_def(
                "get_task_context",
                "Build a ranked, cited context package for a task description: matching symbols with definition excerpts, path-matched files, test references, and linked knowledge records — fitted to a token budget. Deterministic: identical task text and index state yield identical packages. Scores cite their signals in `reasons`; `estimated_tokens` never exceeds the budget except for a single over-budget top hit (noted). Prefer this over repeated `definition`/`search` exploration at the start of a task.",
                json!({
                    "type": "object",
                    "properties": {
                        "task": {
                            "type": "string",
                            "description": "Free-form task text (e.g. Add refund support to payments)"
                        },
                        "token_budget": {
                            "type": "integer",
                            "description": "Token budget the package must fit (100-100000, default 8000)"
                        },
                        "include_tests": {
                            "type": "boolean",
                            "description": "Include test files referencing matched symbols (default true)"
                        },
                        "include_knowledge": {
                            "type": "boolean",
                            "description": "Include linked knowledge records (default true)"
                        }
                    },
                    "required": ["task"]
                }),
                true,
            ),
            tool_def(
                "explain",
                "Explain a symbol, file, or module with a deterministic source-linked evidence package: definition + excerpt, callers, dependencies, dependents, implementations, and linked knowledge records with freshness verdicts — plus content-hash pins so cached copies can detect drift. Pass exactly one of `name` (with optional `module` filter), `file`, or `module`. Ambiguous names return `candidates` to narrow; `generated` is always false from this tool (an LLM renderer sets it with its model id).",
                json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Symbol name to explain (exact, case-sensitive; Type.member supported)"
                        },
                        "file": {
                            "type": "string",
                            "description": "Indexed file path to explain instead of a symbol (e.g. src/auth.ts)"
                        },
                        "module": {
                            "type": "string",
                            "description": "Module to explain (when name is absent), or to disambiguate name (e.g. crate::mcp)"
                        }
                    },
                    "required": []
                }),
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

    // Tools whose correctness depends on index freshness auto-index first.
    // `knowledge_validate` reads anchors; `knowledge_add` is a pure store
    // write with no index dependency, so it stays out.
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
            | "knowledge_search"
            | "knowledge_get"
            | "knowledge_status"
            | "knowledge_validate"
            | "get_task_context"
            | "explain"
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
        "knowledge_search" => {
            let query = require_symbol_arg(&arguments, "query")?;
            let effective = optional_limit_arg(&arguments)
                .unwrap_or(knowledge::DEFAULT_SEARCH_LIMIT)
                .clamp(1, knowledge::MAX_SEARCH_LIMIT);
            let hits = knowledge::search_records(conn, &query, Some(effective))?;
            let freshness: std::collections::HashMap<String, (bool, Vec<String>)> =
                knowledge::check_freshness(conn)?
                    .into_iter()
                    .map(|f| (f.id, (f.fresh, f.reasons)))
                    .collect();
            let results: Vec<Value> = hits
                .iter()
                .map(|r| {
                    let (fresh, reasons) =
                        freshness.get(&r.id).cloned().unwrap_or((true, Vec::new()));
                    json!({"record": r, "fresh": fresh, "reasons": reasons})
                })
                .collect();
            let mut notes = Vec::new();
            // Exact total when uncapped; a recount (only in the capped case)
            // keeps the cap note honest instead of guessing.
            let total = if hits.len() < effective {
                hits.len()
            } else {
                let full =
                    knowledge::search_records(conn, &query, Some(knowledge::MAX_SEARCH_LIMIT))?;
                if full.len() < knowledge::MAX_SEARCH_LIMIT {
                    notes.push(format!(
                        "Showing {effective} of {} matches; narrow the query or raise `limit`.",
                        full.len()
                    ));
                } else {
                    notes.push(format!(
                        "At the {}-match cap; narrow the query.",
                        knowledge::MAX_SEARCH_LIMIT
                    ));
                }
                full.len()
            };
            json_text(json!({"results": results, "total": total, "notes": notes}))?
        }
        "knowledge_get" => {
            let id = require_symbol_arg(&arguments, "id")?;
            match knowledge::get_record(conn, &id)? {
                Some(record) => {
                    let (fresh, reasons) = knowledge::check_one(conn, &id)?
                        .map(|f| (f.fresh, f.reasons))
                        .unwrap_or((true, Vec::new()));
                    json_text(json!({"record": record, "fresh": fresh, "reasons": reasons}))?
                }
                None => {
                    let mut notes = vec![format!("No knowledge record with id `{id}`.")];
                    if let Some(hint) = suggest_knowledge_id(conn, &id)? {
                        notes.push(hint);
                    }
                    json_text(json!({"record": Value::Null, "notes": notes}))?
                }
            }
        }
        "knowledge_status" => {
            let id = arguments
                .get("id")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty());
            match id {
                Some(id) => match knowledge::check_one(conn, &id)? {
                    Some(verdict) => json_text(json!({"records": [verdict], "notes": []}))?,
                    None => json_text(
                        json!({"records": [], "notes": [format!("No knowledge record with id `{id}`.")]}),
                    )?,
                },
                None => {
                    let records = knowledge::check_freshness(conn)?;
                    json_text(json!({"records": records, "notes": []}))?
                }
            }
        }
        "knowledge_add" => {
            let raw = arguments.get("record").cloned().ok_or_else(|| {
                KeelError::Mcp("missing required argument: record".into())
            })?;
            let record: KnowledgeRecord = serde_json::from_value(raw)
                .map_err(|e| KeelError::Mcp(format!("invalid record: {e}")))?;
            let created = knowledge::get_record(conn, &record.id)?.is_none();
            knowledge::put_record(conn, &record)?;
            let stored = knowledge::get_record(conn, &record.id)?.ok_or_else(|| {
                KeelError::Mcp(format!("record `{}` lost after write", record.id))
            })?;
            let verb = if created { "Created" } else { "Updated" };
            json_text(json!({
                "record": stored,
                "created": created,
                "notes": [format!("{verb} knowledge record `{}`.", stored.id)],
            }))?
        }
        "knowledge_validate" => {
            let id = require_symbol_arg(&arguments, "id")?;
            let outcome = knowledge::validate_record(conn, &id)?;
            json_text(outcome)?
        }
        "explain" => {
            let name = arguments
                .get("name")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty());
            let file = arguments
                .get("file")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
                .filter(|s| !s.is_empty());
            let module = optional_module_arg(&arguments);
            let options = explain::ExplainOptions {
                source_root: Some(root.to_path_buf()),
            };
            let explanation = match (name, file, module) {
                (Some(_), Some(_), _) | (None, Some(_), Some(_)) | (None, None, None) => {
                    return Err(KeelError::Mcp(
                        "pass exactly one of name, file, or module".into(),
                    ))
                }
                (Some(name), None, module) => {
                    explain::explain_symbol(conn, &name, module.as_deref(), &options)?
                }
                (None, Some(file), None) => explain::explain_file(conn, &file)?,
                (None, None, Some(module)) => explain::explain_module(conn, &module)?,
            };
            json_text(explanation)?
        }
        "get_task_context" => {
            let task = require_symbol_arg(&arguments, "task")?;
            let budget = arguments
                .get("token_budget")
                .and_then(|v| v.as_u64())
                .and_then(|n| usize::try_from(n).ok());
            let include_tests = arguments
                .get("include_tests")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let include_knowledge = arguments
                .get("include_knowledge")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let options = context::ContextOptions {
                token_budget: budget.unwrap_or(context::DEFAULT_TOKEN_BUDGET),
                include_tests,
                include_knowledge,
                source_root: Some(root.to_path_buf()),
            };
            let package = context::build_package(conn, &task, &options)?;
            json_text(package)?
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

/// Up to 3 known record ids containing the missed id (case-insensitive),
/// ordered by id. `None` when nothing resembles the miss.
fn suggest_knowledge_id(conn: &Connection, id: &str) -> Result<Option<String>> {
    let q = id.to_lowercase();
    let hits: Vec<String> = knowledge::list_records(conn)?
        .into_iter()
        .map(|r| r.id)
        .filter(|known| known.to_lowercase().contains(&q))
        .take(3)
        .collect();
    if hits.is_empty() {
        return Ok(None);
    }
    let quoted: Vec<String> = hits.iter().map(|s| format!("`{s}`")).collect();
    Ok(Some(format!("Did you mean {}?", quoted.join(", "))))
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
            "knowledge_search",
            "knowledge_get",
            "knowledge_status",
            "knowledge_add",
            "knowledge_validate",
            "get_task_context",
            "explain",
        ] {
            assert!(
                names.contains(&expected),
                "missing tool {expected}; have {names:?}"
            );
        }
        // Read-only knowledge tools advertise it; write tools must not.
        for tool_name in [
            "knowledge_search",
            "knowledge_get",
            "knowledge_status",
            "get_task_context",
            "explain",
        ] {
            let tool = tools
                .iter()
                .find(|t| t["name"] == tool_name)
                .unwrap_or_else(|| panic!("{tool_name} tool"));
            assert_eq!(
                tool["annotations"]["readOnlyHint"],
                true,
                "{tool_name} should be readOnly for agent approval UX"
            );
        }
        for tool_name in ["knowledge_add", "knowledge_validate"] {
            let tool = tools
                .iter()
                .find(|t| t["name"] == tool_name)
                .unwrap_or_else(|| panic!("{tool_name} tool"));
            assert!(
                tool.get("annotations").is_none(),
                "{tool_name} writes and must not claim readOnly"
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

    /// Call one tool and parse its text payload as JSON.
    fn call_tool_payload(
        conn: &mut Connection,
        root: &Path,
        name: &str,
        arguments: Value,
        id: i64,
    ) -> Value {
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        });
        let resp = handle_message_with(conn, &msg, false, root)
            .expect("tool call")
            .expect("tool must return a response");
        let text = resp["result"]["content"][0]["text"]
            .as_str()
            .expect("text payload");
        serde_json::from_str(text).unwrap()
    }

    /// Call one tool and return the raw JSON-RPC response (for error cases).
    fn call_tool_raw(
        conn: &mut Connection,
        root: &Path,
        name: &str,
        arguments: Value,
        id: i64,
    ) -> Value {
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {"name": name, "arguments": arguments}
        });
        handle_message_with(conn, &msg, false, root)
            .expect("handler runs")
            .expect("response")
    }

    fn knowledge_db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        (dir, conn)
    }

    fn sample_record_json(id: &str, origin: &str) -> Value {
        json!({
            "id": id,
            "type": "architecture_fact",
            "title": format!("Title for {id}"),
            "content": format!("Claim about {id}"),
            "origin": origin,
            "status": "unverified",
            "evidence": [{
                "source_path": "src/a.rs",
                "symbol": "alpha",
            }],
        })
    }

    #[test]
    fn knowledge_add_get_roundtrip_with_provenance_and_freshness() {
        let (dir, mut conn) = knowledge_db();
        let root = dir.path();

        let added = call_tool_payload(
            &mut conn,
            root,
            "knowledge_add",
            json!({"record": sample_record_json("auth.jwt", "agent_generated")}),
            1,
        );
        assert_eq!(added["created"], true, "got: {added}");
        assert_eq!(added["record"]["id"], "auth.jwt");
        assert_eq!(added["record"]["origin"], "agent_generated");
        // Store-stamped timestamps, not caller values.
        assert!(!added["record"]["created_at"].as_str().unwrap_or("").is_empty());

        let got = call_tool_payload(
            &mut conn,
            root,
            "knowledge_get",
            json!({"id": "auth.jwt"}),
            2,
        );
        assert_eq!(got["record"]["id"], "auth.jwt");
        assert_eq!(got["record"]["origin"], "agent_generated");
        assert_eq!(got["record"]["status"], "unverified");
        assert_eq!(got["record"]["evidence"][0]["symbol"], "alpha");
        // Every response carries freshness metadata.
        assert!(got.get("fresh").is_some(), "got: {got}");
        assert!(got.get("reasons").is_some(), "got: {got}");
    }

    #[test]
    fn knowledge_get_miss_is_null_with_suggestion() {
        let (dir, mut conn) = knowledge_db();
        let root = dir.path();
        call_tool_payload(
            &mut conn,
            root,
            "knowledge_add",
            json!({"record": sample_record_json("auth.jwt.validation", "agent_generated")}),
            1,
        );

        let miss = call_tool_payload(&mut conn, root, "knowledge_get", json!({"id": "auth"}), 2);
        assert!(miss["record"].is_null(), "got: {miss}");
        assert_eq!(miss["notes"][0], "No knowledge record with id `auth`.");
        assert!(
            miss["notes"][1].as_str().unwrap_or("").contains("auth.jwt.validation"),
            "got: {miss}"
        );

        let cold = call_tool_payload(&mut conn, root, "knowledge_get", json!({"id": "zzz"}), 3);
        assert!(cold["record"].is_null());
        assert_eq!(cold["notes"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn knowledge_search_ranks_and_notes_caps() {
        let (dir, mut conn) = knowledge_db();
        let root = dir.path();
        for id in ["my.jwt.store", "jwt.suffix", "jwt"] {
            call_tool_payload(
                &mut conn,
                root,
                "knowledge_add",
                json!({"record": sample_record_json(id, "agent_generated")}),
                1,
            );
        }

        let payload = call_tool_payload(
            &mut conn,
            root,
            "knowledge_search",
            json!({"query": "jwt"}),
            2,
        );
        let ids: Vec<&str> = payload["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["record"]["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, vec!["jwt", "jwt.suffix", "my.jwt.store"]);
        assert_eq!(payload["total"], 3);
        assert!(payload["notes"].as_array().unwrap().is_empty());
        assert!(payload["results"][0].get("fresh").is_some());

        let capped = call_tool_payload(
            &mut conn,
            root,
            "knowledge_search",
            json!({"query": "jwt", "limit": 1}),
            3,
        );
        assert_eq!(capped["results"].as_array().unwrap().len(), 1);
        assert_eq!(capped["total"], 3);
        assert!(
            capped["notes"][0].as_str().unwrap_or("").contains("Showing 1 of 3"),
            "got: {capped}"
        );
    }

    #[test]
    fn knowledge_status_all_one_and_unknown() {
        let (dir, mut conn) = knowledge_db();
        let root = dir.path();
        call_tool_payload(
            &mut conn,
            root,
            "knowledge_add",
            json!({"record": sample_record_json("k1", "human")}),
            1,
        );

        let all = call_tool_payload(&mut conn, root, "knowledge_status", json!({}), 2);
        assert_eq!(all["records"].as_array().unwrap().len(), 1);
        assert_eq!(all["records"][0]["id"], "k1");

        let one = call_tool_payload(&mut conn, root, "knowledge_status", json!({"id": "k1"}), 3);
        assert_eq!(one["records"].as_array().unwrap().len(), 1);

        let unknown =
            call_tool_payload(&mut conn, root, "knowledge_status", json!({"id": "nope"}), 4);
        assert!(unknown["records"].as_array().unwrap().is_empty());
        assert_eq!(unknown["notes"][0], "No knowledge record with id `nope`.");
    }

    #[test]
    fn knowledge_validate_promotes_when_anchors_resolve() {
        use crate::db::queries;
        use crate::graph::types::{FileNode, Symbol, SymbolKind};

        let (dir, mut conn) = knowledge_db();
        let root = dir.path();
        let hash = "b".repeat(64);
        let file_id = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("src/a.rs"),
                content_hash: hash.clone(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            &conn,
            file_id,
            &[Symbol {
                name: "alpha".into(),
                kind: SymbolKind::Function,
                file: PathBuf::new(),
                start_line: 1,
                start_col: 1,
                module_path: String::new(),
                container: String::new(),
            }],
        )
        .unwrap();
        call_tool_payload(
            &mut conn,
            root,
            "knowledge_add",
            json!({"record": sample_record_json("k1", "agent_generated")}),
            1,
        );

        let outcome = call_tool_payload(
            &mut conn,
            root,
            "knowledge_validate",
            json!({"id": "k1"}),
            2,
        );
        assert_eq!(outcome["outcome"], "validated", "got: {outcome}");
        assert_eq!(outcome["repinned"], 1);

        // Pins now match the live index: get reports supported + fresh.
        let got = call_tool_payload(&mut conn, root, "knowledge_get", json!({"id": "k1"}), 3);
        assert_eq!(got["record"]["status"], "supported");
        assert_eq!(got["record"]["evidence"][0]["source_hash"], hash);
        assert_eq!(got["fresh"], true);
    }

    #[test]
    fn get_task_context_returns_cited_budgeted_package() {
        use crate::db::queries;
        use crate::graph::types::{
            FileNode, Reference, ReferenceKind, Symbol, SymbolKind,
        };

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(
            root.join("src/a.py"),
            "def refund(x):\n    return -x\n\ndef refund_all(xs):\n    return [-x for x in xs]\n",
        )
        .unwrap();
        std::fs::write(
            root.join("tests/test_a.py"),
            "from src.a import refund\nassert refund(1)\n",
        )
        .unwrap();
        let mut conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        let src_id = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("src/a.py"),
                content_hash: "h".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            &conn,
            src_id,
            &[
                Symbol {
                    name: "refund".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: 1,
                    start_col: 1,
                    module_path: String::new(),
                    container: String::new(),
                },
                Symbol {
                    name: "refund_all".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: 4,
                    start_col: 1,
                    module_path: String::new(),
                    container: String::new(),
                },
            ],
        )
        .unwrap();
        let test_id = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("tests/test_a.py"),
                content_hash: "h".into(),
            },
        )
        .unwrap();
        queries::insert_references(
            &conn,
            test_id,
            &[Reference {
                name: "refund".into(),
                file: PathBuf::new(),
                start_line: 2,
                start_col: 8,
                kind: ReferenceKind::Call,
                container: String::new(),
                qualifier: String::new(),
            }],
        )
        .unwrap();

        let payload = call_tool_payload(
            &mut conn,
            root,
            "get_task_context",
            json!({"task": "Add refund support"}),
            1,
        );
        assert_eq!(payload["task"], "Add refund support");
        assert!(payload["estimated_tokens"].as_u64().unwrap() <= 8000);
        let items = payload["items"].as_array().unwrap();
        assert!(!items.is_empty());
        assert_eq!(items[0]["kind"], "symbol");
        assert_eq!(items[0]["name"], "refund");
        assert!(
            items[0]["text"].as_str().unwrap_or("").contains("def refund"),
            "got: {payload}"
        );
        assert!(items.iter().any(|i| i["kind"] == "test"), "got: {payload}");

        // Toggles and budgets are honored.
        let no_tests = call_tool_payload(
            &mut conn,
            root,
            "get_task_context",
            json!({"task": "Add refund support", "include_tests": false}),
            2,
        );
        assert!(no_tests["items"].as_array().unwrap().iter().all(|i| i["kind"] != "test"));

        let tight = call_tool_payload(
            &mut conn,
            root,
            "get_task_context",
            json!({"task": "Add refund support", "token_budget": 100}),
            3,
        );
        assert_eq!(tight["token_budget"], 100);
        assert!(tight["omitted"].as_u64().unwrap() > 0, "got: {tight}");
    }

    #[test]
    fn explain_tool_returns_package_disambiguation_and_conflicts() {
        use crate::db::queries;
        use crate::graph::types::{FileNode, Symbol, SymbolKind};

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/a.py"), "def solo(x):\n    return x\n").unwrap();
        std::fs::write(root.join("src/b.py"), "def solo(y):\n    return y\n").unwrap();
        let mut conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        for (path, line) in [("src/a.py", 1), ("src/b.py", 1)] {
            let file_id = queries::insert_file(
                &conn,
                &FileNode {
                    path: PathBuf::from(path),
                    content_hash: "h".into(),
                },
            )
            .unwrap();
            queries::insert_symbols(
                &conn,
                file_id,
                &[Symbol {
                    name: "solo".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: line,
                    start_col: 1,
                    module_path: String::new(),
                    container: String::new(),
                }],
            )
            .unwrap();
        }

        // Ambiguous without a filter: candidates, no definition.
        let ambiguous = call_tool_payload(
            &mut conn,
            root,
            "explain",
            json!({"name": "solo"}),
            1,
        );
        assert_eq!(ambiguous["disambiguation"].as_array().unwrap().len(), 2);
        assert!(ambiguous["definition"].is_null());
        assert_eq!(ambiguous["generated"], false);

        // Conflicting targets fail loudly.
        let resp = call_tool_raw(
            &mut conn,
            root,
            "explain",
            json!({"name": "solo", "file": "src/a.py"}),
            2,
        );
        assert!(
            resp["error"]["message"].as_str().unwrap_or("").contains("exactly one"),
            "got: {resp}"
        );

        // File form carries the outline + pins.
        let file = call_tool_payload(&mut conn, root, "explain", json!({"file": "src/a.py"}), 3);
        assert_eq!(file["file"]["path"], "src/a.py");
        assert_eq!(file["file"]["symbols_total"], 1);
        assert!(file["source_hashes"].get("src/a.py").is_some());
    }

    #[test]
    fn knowledge_add_rejects_human_overwrite_and_invalid_records() {
        let (dir, mut conn) = knowledge_db();
        let root = dir.path();
        call_tool_payload(
            &mut conn,
            root,
            "knowledge_add",
            json!({"record": sample_record_json("adr.1", "human")}),
            1,
        );

        let resp = call_tool_raw(
            &mut conn,
            root,
            "knowledge_add",
            json!({"record": sample_record_json("adr.1", "agent_generated")}),
            2,
        );
        let message = resp["error"]["message"].as_str().unwrap_or("");
        assert!(message.contains("refusing"), "got: {resp}");

        let mut bad = sample_record_json("has space", "agent_generated");
        bad["title"] = json!("");
        let resp = call_tool_raw(
            &mut conn,
            root,
            "knowledge_add",
            json!({"record": bad}),
            3,
        );
        let message = resp["error"]["message"].as_str().unwrap_or("");
        assert!(message.contains("invalid"), "got: {resp}");
    }
}
