# Keel 1.1 (library & contributors)

Deterministic, local-first code intelligence for AI coding agents. Keel
indexes a repository with Tree-sitter and answers structural queries from a
local SQLite database — no LLMs, embeddings, or semantic search.

**For install, Cursor MCP, and everyday CLI usage, start at the
[repository-root README](../README.md).** This file covers the Rust library,
language plugins, protocol details, and building from source.

**1.1** adds JavaScript/JSX and Python indexing plus binary distribution via
curl and Homebrew. Semver guarantees apply to the public crate surface
(especially [`Index`](#library-api-index)) and the documented CLI / MCP / HTTP
contracts.

## Install (summary)

```bash
brew tap ashokdudhade/keel https://github.com/ashokdudhade/keel
brew install ashokdudhade/keel/keel
# or: curl -fsSL https://raw.githubusercontent.com/ashokdudhade/keel/main/install.sh | sh
```

Then: `brew services start keel` once per machine, `keel start` in each
project, and configure Cursor with `KEEL_INDEX_DB` as documented in the
[root README](../README.md#quick-start-cursor).

### Build from source (contributors)

Requires Git, Rust stable + Cargo ([rustup](https://rustup.rs/)), and a C
toolchain for bundled SQLite:

- macOS: `xcode-select --install`
- Debian/Ubuntu: `sudo apt install build-essential`
- Fedora: `sudo dnf groupinstall "Development Tools"`

```bash
git clone https://github.com/ashokdudhade/keel.git
cd keel
cargo install --path ./keel
# or: cargo build --release && ./target/release/keel --help
```

On macOS, if a build reports an unaccepted Xcode license while Command Line
Tools are already installed:

```bash
DEVELOPER_DIR=/Library/Developer/CommandLineTools cargo build --release
```

## Index layout

Per project: `.keel/index.db` (add `.keel/` to `.gitignore`).  
Daemon state: `~/.keel/daemon/` (`KEEL_HOME`).  
Control API: `127.0.0.1:7646` (`KEEL_DAEMON_PORT` / `keel daemon --port`).

Recommended flow (global daemon + project registration):

```bash
brew services start keel   # or: keel daemon
cd /path/to/project && keel start
```

Without the daemon: `keel index .` / `keel watch .`.  
Queries auto-run a fast incremental index unless `--no-auto-index` is set.

Reset: `rm -rf .keel` then `keel start` or `keel index .`.

## CLI reference

```bash
keel daemon [--port 7646]         # global daemon (brew services start keel)
keel start [path]                 # register project: index + watch
keel stop                         # unregister this project
keel status                       # daemon + this project
keel index <path>                 # one-shot index into ./.keel/index.db
keel watch <path>                 # foreground re-index on file changes
keel definition <name>
keel references <name>
keel callers <name>
keel implementations <trait>
keel dependencies <name|module>
keel impact <name>
keel serve [--port 7645]          # JSON HTTP API on 127.0.0.1
keel mcp                          # MCP stdio (NDJSON; Content-Length also accepted)
```

CLI output is `path:line:col` (1-based), tab-separated, stable and script-friendly.

## Supported source files

| Language | Extensions | Notes |
|----------|------------|-------|
| Rust | `.rs` | Module paths derive from the crate / `mod` hierarchy |
| TypeScript / TSX | `.ts`, `.tsx`, `.mts`, `.cts` | Module paths derive from source file paths |
| JavaScript / JSX | `.js`, `.jsx`, `.mjs`, `.cjs` | ESM + literal CommonJS `require`; path modules |
| Python | `.py`, `.pyi` | Package-style dotted module paths |
| Go | `.go` | Package names; `package main` is path-qualified |

One indexing pass can process all supported languages in a mixed monorepo.
`keel watch` reacts to changes on registered extensions.

## MCP (`keel mcp`)

Stdio MCP server over the index. Prefer an absolute DB path for editors:

| Variable / config | Purpose |
|-------------------|---------|
| `KEEL_INDEX_DB` | Absolute path to `index.db` (**recommended** for Cursor) |
| `cwd` + `./.keel/index.db` | Fallback when the client honors working directory |
| `KEEL_MCP_DEBUG` | Re-enable stderr diagnostics |

Wire format: **newline-delimited JSON-RPC** (Cursor `2025-11-25` and similar).
Older **Content-Length** framing is still accepted. Logs go to stderr only.

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

Also handles `initialize`, `tools/list`, `tools/call`, `ping`, and empty
`resources/list` / `prompts/list` / `resources/templates/list` for client probes.

Cursor setup (copy-paste template): see
[root README — Quick start (Cursor)](../README.md#quick-start-cursor).

## JSON API (`keel serve`)

Default: `http://127.0.0.1:7645`.

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

## Install as a Rust library

For local development against this checkout:

```toml
[dependencies]
keel = { path = "/absolute/path/to/keel/keel" }
```

If published to your Cargo registry:

```toml
[dependencies]
keel = "1.1"
```

## Library API (`Index`)

```rust
use keel::{Index, Registry, LanguagePlugin};
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

## Language notes

TypeScript module paths derive from source file paths. Go uses package names,
with path-based identity for `package main`. Go has no `impl Trait for Type`
form, so `implementations` queries stay empty for Go; interface satisfaction is
future work.

**Impact** is name-based transitive expansion over reference containers; cycles
terminate, but overloaded names can over-approximate.

## Community language plugins

Built-ins are registered via `Registry::with_defaults()`. External crates can
ship plugins:

```rust
use keel::{index_repository_with, LanguagePlugin, Registry};

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

## Resolution model

Cross-file resolution uses an in-house **module/import-aware deterministic
resolver** over the SQLite index (not ML):

1. Exact `module_path::name` reachable via an `imports` row in the caller's file
2. Same-module match
3. Fall back to all name matches (v0.1 behavior)

Within a tier, results are ordered by `(path, line, col)`.

## Troubleshooting (contributor)

### Build fails while compiling SQLite

Install a C compiler (see [Build from source](#build-from-source-contributors)).
On macOS: `xcode-select -p`.

### Index reports file errors

Keel continues indexing readable files. Review stderr for paths that were
unreadable, invalid UTF-8, or otherwise failed extraction.

End-user PATH / daemon / MCP issues: [root README troubleshooting](../README.md#troubleshooting).

## Uninstall

```bash
brew services stop keel; brew uninstall keel
rm -f "${KEEL_INSTALL_DIR:-$HOME/.local/bin}/keel"
cargo uninstall keel   # if installed from source
rm -rf /path/to/project/.keel
```

## Scope (1.1)

- Stable library API: `Index`, `Registry` / `LanguagePlugin`, graph query types
- Languages: Rust, TypeScript/TSX, JavaScript/JSX, Python, Go
- Community plugin registration (`Registry::register`)
- MCP stdio server and JSON HTTP API
- Global daemon + per-project `start` / `stop` / `status`
- Incremental indexing via content hashes; live updates via daemon watch
- Graph queries: implementations, dependencies, transitive impact

## License

Open source (see repository).
