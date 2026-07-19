# SecondBrain

Deterministic, local-first code intelligence for AI coding agents. SecondBrain
indexes a repository with Tree-sitter and answers structural queries from a
local SQLite database — no LLMs, embeddings, or semantic search.

## Install / Build

```bash
cargo build --release
```

The binary is `sb` (target/release/sb).

## Usage

```bash
sb index <path>                 # index a repository into ./.secondbrain/index.db
sb watch <path>                 # re-index on .rs file changes
sb definition <name>            # where a symbol is defined
sb references <name>            # where a name is referenced
sb callers <name>               # call/use sites (import-aware when unique)
sb implementations <trait>      # types that implement a trait
sb dependencies <name|module>   # modules/files a symbol or module depends on
sb impact <name>                # symbols transitively impacted by a change
sb serve [--port 7645]          # JSON HTTP API on 127.0.0.1
sb mcp                          # MCP stdio server (Content-Length JSON-RPC)
```

CLI output is `path:line:col` (1-based), tab-separated, stable and script-friendly.

## Languages (v0.3)

Indexed by extension via the language plugin registry:

| Language   | Extensions              | `module_path` |
|------------|-------------------------|---------------|
| Rust       | `.rs`                   | crate / `mod` chain (e.g. `crate::auth`) |
| TypeScript | `.ts`, `.tsx`, `.mts`, `.cts` | fixed `"module"` (see note) |
| Go         | `.go`                   | package name from `package` clause |

**TypeScript `module_path` limitation:** the plugin API receives source text only
(no file path), so symbols use the fixed string `"module"` rather than a
path-stem like `src/auth/service`. Name-based lookup still works; cross-file
import-aware resolution is weaker for TS than for Rust/Go.

Go has no `impl Trait for Type` form — `implementations` queries stay empty for
Go; interface satisfaction is future work.

## MCP (`sb mcp`)

Starts a stdio MCP server over the index at `./.secondbrain/index.db` (creates
schema if missing). Messages use **Content-Length** framed JSON-RPC 2.0
(stdout for frames; logs on stderr only).

Stable tools:

| Tool | Arguments | Description |
|------|-----------|-------------|
| `definition` | `{ "name" }` | Symbol definition(s) |
| `references` | `{ "name" }` | Reference sites |
| `callers` | `{ "name" }` | Call/use sites (import-aware when unique) |
| `implementations` | `{ "name" }` | Trait implementations |
| `dependencies` | `{ "name" }` | Module/file dependencies |
| `impact` | `{ "name" }` | Transitively impacted symbols |
| `index` | `{ "path" }` | Index a repository; returns `IndexStats` JSON |

Tool results are JSON text content with the same DTO shapes as the HTTP API
where applicable. Also supports `initialize`, `tools/list`, `tools/call`, and
`ping`.

Example Cursor / Claude MCP config (stdio):

```json
{
  "mcpServers": {
    "secondbrain": {
      "command": "/path/to/sb",
      "args": ["mcp"]
    }
  }
}
```

## JSON API (`sb serve`)

Default listen address: `http://127.0.0.1:7645`.

```http
GET /health
```

```json
{"status":"ok"}
```

```http
GET /symbol/{name}
```

```json
{
  "definition": [
    {
      "name": "AuthService",
      "kind": "struct",
      "file": "src/lib.rs",
      "start_line": 1,
      "start_col": 12,
      "module_path": "crate"
    }
  ],
  "references": [],
  "implementations": [],
  "dependencies": [],
  "callers": []
}
```

Arrays are ordered deterministically. File paths are JSON strings.

## Resolution model (v0.2+)

Cross-file resolution uses an in-house **module/import-aware deterministic
resolver** over the SQLite index (not ML):

1. Exact `module_path::name` reachable via an `imports` row in the caller's file
2. Same-module match
3. Fall back to all name matches (v0.1 behavior)

Within a tier, results are ordered by `(path, line, col)`.

**Stack Graphs** remains a viable alternative resolution backend for a future
precision layer; v0.2 ships the deterministic AST/module resolver instead
because a maintained published Stack Graphs Rust rules crate was not available.

## Scope

- Languages: Rust, TypeScript/TSX, Go (v0.3).
- MCP stdio server for AI agents (`sb mcp`).
- Incremental indexing via content hashes; `sb watch` for live updates (Rust
  file events; re-index still picks up all registered extensions).
- Graph queries: implementations, dependencies, transitive impact.

## License

Open source (see repository).
