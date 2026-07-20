# Keel 1.0

Deterministic, local-first code intelligence for AI coding agents. Keel
indexes a repository with Tree-sitter and answers structural queries from a
local SQLite database — no LLMs, embeddings, or semantic search.

**1.0 is the stable library + CLI release:** semver guarantees apply to the
public crate surface (especially [`Index`](#library-api-index)) and the
documented CLI / MCP / HTTP contracts.

## Keel 1.1 roadmap

The approved v1.1 design adds JavaScript/JSX and Python/Python-stub indexing,
plus prebuilt macOS/Linux binaries distributed through a verified curl
installer and Homebrew formula.

Until the `v1.1.0` release assets are published, install Keel from source with
Cargo. See the repository-root README for the active installation commands.

## Prerequisites

- Git
- Rust stable and Cargo ([install with rustup](https://rustup.rs/))
- A C toolchain for bundled SQLite:
  - macOS: `xcode-select --install`
  - Debian/Ubuntu: `sudo apt install build-essential`
  - Fedora: `sudo dnf groupinstall "Development Tools"`

Verify the toolchain:

```bash
rustc --version
cargo --version
```

## Install locally

### Option 1: Install the `keel` command (recommended)

Clone the repository and install from its Rust package:

```bash
git clone https://github.com/ashokdudhade/keel.git
cd keel
cargo install --path ./keel
```

Cargo installs `keel` into `~/.cargo/bin`. Ensure that directory is on `PATH`:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
keel --help
```

To make the PATH change permanent, add the `export` line to `~/.zshrc`,
`~/.bashrc`, or the configuration file used by your shell.

Reinstall after pulling a newer version:

```bash
git pull
cargo install --path ./keel --force
```

### Option 2: Build without installing

From the cloned repository root:

```bash
cd keel
cargo build --release
```

The executable is `target/release/keel`. Run it directly or copy it somewhere on
your PATH:

```bash
./target/release/keel --help
```

On macOS, if a build reports an unaccepted Xcode license while Command Line
Tools are already installed, either accept the license or build with:

```bash
DEVELOPER_DIR=/Library/Developer/CommandLineTools cargo build --release
```

## Quick start

Change to the repository you want to inspect. Keel stores its local index
in `.keel/index.db` under the **current working directory**.

```bash
cd /path/to/your/project

# Build or refresh the index.
keel index .

# Query it.
keel definition AuthService
keel references create_order
keel callers create_order
```

Example output:

```text
src/auth/service.rs:12:12	struct	AuthService
```

Positions are 1-based. Query rows are deterministic, tab-separated, and suitable
for scripts.

Add the generated index directory to the target project's `.gitignore`:

```gitignore
.keel/
```

## Typical local workflow

### 1. Index a project

```bash
cd /path/to/project
keel index .
```

The first run parses all supported source files. Later runs compare content
hashes, skip unchanged files, update changed files, and remove deleted files
from the index.

Example status:

```text
Indexed 3 file(s) (skipped 41, removed 1, errors 0).
```

If `errors` is non-zero, successfully read files remain indexed; inspect stderr
for skipped-file diagnostics.

### 2. Query code structure

```bash
keel definition <symbol>
keel references <symbol>
keel callers <function>
keel implementations <trait>
keel dependencies <symbol-or-module>
keel impact <symbol>
```

Examples:

```bash
keel definition IndexStats
keel references index_repository
keel implementations LanguagePlugin
keel dependencies crate::index
keel impact index_repository
```

### 3. Keep the index current

Run the watcher from the target project:

```bash
keel watch .
```

Leave it running while editing. Stop it with `Ctrl-C`. You can also omit the
watcher and rerun `keel index .` whenever needed.

### 4. Reset or remove an index

The index contains derived data only and is safe to delete:

```bash
rm -rf .keel
keel index .
```

Rebuild indexes created before a Keel upgrade if migration or path-format
notes in the changelog recommend it.

## Use with Cursor through MCP

First index the project:

```bash
cd /path/to/your/project
keel index .
which keel
```

Use the absolute path printed by `which keel` in Cursor's MCP configuration. The
`cwd` must be the indexed project because `keel mcp` opens
`./.keel/index.db`.

```json
{
  "mcpServers": {
    "keel": {
      "command": "/absolute/path/to/keel",
      "args": ["mcp"],
      "cwd": "/absolute/path/to/your/project"
    }
  }
}
```

Restart or refresh MCP servers in Cursor after saving the configuration.
Keel then exposes these tools:

- `definition`
- `references`
- `callers`
- `implementations`
- `dependencies`
- `impact`
- `index`

Example prompts to validate the integration:

```text
Use Keel to find the definition and references of IndexStats.
Use Keel to show callers of index_repository.
Use Keel to estimate the impact of changing LanguagePlugin.
```

If Cursor cannot start the server, run the exact configured command manually
from the configured `cwd`:

```bash
cd /absolute/path/to/your/project
/absolute/path/to/keel mcp
```

The process waits for MCP messages on stdin; no normal prompt is expected.

## Run the local JSON API

From an indexed project:

```bash
keel serve --port 7645
```

Test it from another terminal:

```bash
curl http://127.0.0.1:7645/health
curl http://127.0.0.1:7645/symbol/AuthService
```

The server binds to loopback (`127.0.0.1`) and is not exposed to other machines
by default.

## Supported source files

| Language | Extensions | Notes |
|----------|------------|-------|
| Rust | `.rs` | Module paths derive from the crate / `mod` hierarchy |
| TypeScript / TSX | `.ts`, `.tsx`, `.mts`, `.cts` | Module paths derive from source file paths |
| Go | `.go` | Module paths use package names; `package main` is path-qualified |
| JavaScript / JSX | `.js`, `.jsx`, `.mjs`, `.cjs` | Planned for v1.1; ESM and CommonJS |
| Python | `.py`, `.pyi` | Planned for v1.1; package-style module paths |

One indexing pass can process all supported languages in a mixed monorepo.
JavaScript and Python become built-in languages with Keel 1.1.

## Troubleshooting

### `keel: command not found`

Ensure Cargo's bin directory is on PATH:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
```

### Build fails while compiling SQLite

Install a C compiler using the prerequisite instructions above. On macOS, check:

```bash
xcode-select -p
```

### Queries return no results

1. Confirm you are in the same directory used for indexing.
2. Rerun `keel index .`.
3. Check that the file extension is supported and not excluded by `.gitignore`.
4. Try the exact, case-sensitive symbol name.
5. Rebuild the index with `rm -rf .keel && keel index .`.

### MCP starts but tools return empty results

The MCP process probably has the wrong working directory. Its `cwd` must contain
the `.keel/index.db` created for that project.

### Index reports file errors

Keel continues indexing readable files. Review stderr for paths that were
unreadable, invalid UTF-8, or otherwise failed extraction.

## Uninstall

If installed with Cargo:

```bash
cargo uninstall keel
```

Remove per-project indexes separately:

```bash
rm -rf /path/to/project/.keel
```

## Install as a Rust library

For local development against this checkout:

```toml
[dependencies]
keel = { path = "/absolute/path/to/keel/keel" }
```

If the crate is published to your configured Cargo registry, use its published
version instead:

```toml
[dependencies]
keel = "1.0"
```

## CLI reference

```bash
keel index <path>                 # index a repository into ./.keel/index.db
keel watch <path>                 # re-index on registered source file changes
keel definition <name>            # where a symbol is defined
keel references <name>            # where a name is referenced
keel callers <name>               # call/use sites (import-aware when unique)
keel implementations <trait>      # types that implement a trait
keel dependencies <name|module>   # modules/files a symbol or module depends on
keel impact <name>                # symbols transitively impacted by a change
keel serve [--port 7645]          # JSON HTTP API on 127.0.0.1
keel mcp                          # MCP stdio server (Content-Length JSON-RPC)
```

CLI output is `path:line:col` (1-based), tab-separated, stable and script-friendly.

## Library API (`Index`)

Stable 1.0 entry point for embedding Keel in tools and agents:

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

## Multi-language monorepos

A single `index` / `Index::index_path` pass crawls **all** extensions registered
in the language registry (not Rust-only). One repository tree can mix Rust,
TypeScript/TSX, and Go; symbols from each language land in the same SQLite index.

`keel watch` reacts to changes on registered extensions (`.rs`, `.ts`/`.tsx`/…,
`.go`) and re-runs the same incremental indexer.

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

## MCP (`keel mcp`)

Starts a stdio MCP server over the index at `./.keel/index.db` (creates
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

Minimal Cursor / Claude MCP config (stdio). See
[Use with Cursor through MCP](#use-with-cursor-through-mcp) for the required
working directory:

```json
{
  "mcpServers": {
    "keel": {
      "command": "/path/to/keel",
      "args": ["mcp"],
      "cwd": "/path/to/indexed/project"
    }
  }
}
```

## JSON API (`keel serve`)

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
- Incremental indexing via content hashes; `keel watch` for live updates
- Graph queries: implementations, dependencies, transitive impact

## License

Open source (see repository).
