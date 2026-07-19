# SecondBrain 1.0

Deterministic, local-first code intelligence for AI coding agents. SecondBrain
indexes a repository with Tree-sitter and answers structural queries from a
local SQLite database — no LLMs, embeddings, or semantic search.

**1.0 is the stable library + CLI release:** semver guarantees apply to the
public crate surface (especially [`Index`](#library-api-index)) and the
documented CLI / MCP / HTTP contracts.

## Install / Build

```bash
cargo build --release
```

The binary is `sb` (`target/release/sb`).

As a library dependency:

```toml
second_brain = "1.0"
```

## CLI Usage

```bash
sb index <path>                 # index a repository into ./.secondbrain/index.db
sb watch <path>                 # re-index on registered source file changes
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

## Library API (`Index`)

Stable 1.0 entry point for embedding SecondBrain in tools and agents:

```rust
use second_brain::{Index, Registry, LanguagePlugin};
use std::path::Path;

let mut index = Index::open_in_memory()?;
index.index_path(Path::new("./my-repo"))?;

let defs = index.definition("AuthService")?;
let refs = index.references("create_order")?;
let callers = index.callers("create_order")?;
let impls = index.implementations("Storage")?;
let deps = index.dependencies("crate::auth")?;
let impact = index.impact("create_order")?;
```

- `Index::open` / `open_in_memory` — open an on-disk or in-memory index
- `index_path` — index with built-in language plugins
- `index_path_with` — index with a custom [`Registry`](#community-language-plugins)
- Query methods: `definition`, `references`, `callers`, `implementations`,
  `dependencies`, `impact`

Free functions `index_repository` / `index_repository_with` remain available for
callers that already hold a `rusqlite::Connection`.

## Multi-language monorepos

A single `index` / `Index::index_path` pass crawls **all** extensions registered
in the language registry (not Rust-only). One repository tree can mix Rust,
TypeScript/TSX, and Go; symbols from each language land in the same SQLite index.

`sb watch` reacts to changes on registered extensions (`.rs`, `.ts`/`.tsx`/…,
`.go`) and re-runs the same incremental indexer.

## Languages

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

**Impact** is name-based transitive expansion over reference containers; cycles
terminate, but overloaded names can over-approximate.

## Community language plugins

Built-ins are registered via `Registry::with_defaults()`. External crates can
ship plugins:

```rust
use second_brain::{index_repository_with, LanguagePlugin, Registry};

let mut registry = Registry::empty();
registry.register(Box::new(MyPlugin));
// or start from defaults and add more:
// let mut registry = Registry::with_defaults();
// registry.register(Box::new(MyPlugin));

index_repository_with(root, &mut conn, &registry)?;
// Index::index_path_with(root, &registry) is equivalent for facade users.
```

Implement `LanguagePlugin` (`Sync`) with `extensions`, `extract_symbols`,
`extract_references`, and optionally `extract_imports` / `extract_impls`.

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

## Resolution model

Cross-file resolution uses an in-house **module/import-aware deterministic
resolver** over the SQLite index (not ML):

1. Exact `module_path::name` reachable via an `imports` row in the caller's file
2. Same-module match
3. Fall back to all name matches (v0.1 behavior)

Within a tier, results are ordered by `(path, line, col)`.

## Scope (1.0)

- Stable library API: `Index`, `Registry` / `LanguagePlugin`, graph query types
- Languages: Rust, TypeScript/TSX, Go (multi-language monorepos supported)
- Community plugin registration (`Registry::register`)
- MCP stdio server and JSON HTTP API
- Incremental indexing via content hashes; `sb watch` for live updates
- Graph queries: implementations, dependencies, transitive impact

## License

Open source (see repository).
