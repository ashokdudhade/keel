# Keel

Deterministic, local-first code intelligence for AI coding agents. Keel
indexes source code with Tree-sitter and answers structural queries from a local
SQLite database—without LLMs, embeddings, or semantic search.

Supported languages:

- Rust (`.rs`)
- TypeScript / TSX (`.ts`, `.tsx`, `.mts`, `.cts`)
- Go (`.go`)

Interfaces:

- `keel` command-line tool
- MCP stdio server for Cursor and other coding agents
- Local JSON HTTP API
- Stable Rust `Index` library API

## Prerequisites

- Git
- Rust stable and Cargo ([install with rustup](https://rustup.rs/))
- A C compiler for bundled SQLite:
  - macOS: `xcode-select --install`
  - Debian/Ubuntu: `sudo apt install build-essential`
  - Fedora: `sudo dnf groupinstall "Development Tools"`

Verify:

```bash
rustc --version
cargo --version
```

## Install locally

From an existing checkout:

```bash
cd /path/to/keel
cargo install --path ./keel
```

Or clone first:

```bash
git clone <SECOND_BRAIN_REPOSITORY_URL> keel
cd keel
cargo install --path ./keel
```

Cargo installs `keel` into `~/.cargo/bin`. If needed:

```bash
export PATH="$HOME/.cargo/bin:$PATH"
keel --help
```

Add the `export` line to your shell configuration to make it permanent.

To update:

```bash
git pull
cargo install --path ./keel --force
```

### Build without installing

```bash
cd keel
cargo build --release
./target/release/keel --help
```

On macOS, if an Xcode license error occurs despite Command Line Tools being
installed:

```bash
DEVELOPER_DIR=/Library/Developer/CommandLineTools cargo build --release
```

## Quick start

Run Keel from the project you want to inspect:

```bash
cd /path/to/your/project

keel index .
keel definition AuthService
keel references create_order
keel callers create_order
```

The index is stored at:

```text
./.keel/index.db
```

Add this to the target project's `.gitignore`:

```gitignore
.keel/
```

Example query output:

```text
src/auth/service.rs:12:12	struct	AuthService
```

Lines and columns are 1-based. Output ordering is deterministic.

## Commands

```bash
keel index <path>                 # create or incrementally update the index
keel watch <path>                 # re-index when supported files change
keel definition <name>            # find definitions
keel references <name>            # find references
keel callers <name>               # find call/use sites
keel implementations <trait>      # find trait implementations
keel dependencies <name|module>   # find dependencies
keel impact <name>                # estimate transitive impact
keel serve [--port 7645]          # run the local JSON API
keel mcp                          # run the MCP stdio server
```

The index command reports incremental activity:

```text
Indexed 3 file(s) (skipped 41, removed 1, errors 0).
```

Keel continues indexing readable files when an individual file fails.
Details for failed files are written to stderr.

## Keep the index current

Use the watcher while editing:

```bash
cd /path/to/your/project
keel watch .
```

Stop it with `Ctrl-C`. Alternatively, rerun `keel index .` manually.

Reset an index at any time:

```bash
rm -rf .keel
keel index .
```

## Use with Cursor via MCP

Index the target project and locate the executable:

```bash
cd /path/to/your/project
keel index .
which keel
```

Configure Cursor using the absolute paths:

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

The `cwd` is required because `keel mcp` opens
`./.keel/index.db`. Refresh or restart MCP servers after changing the
configuration.

Available MCP tools:

- `definition`
- `references`
- `callers`
- `implementations`
- `dependencies`
- `impact`
- `index`

Example Cursor prompts:

```text
Use Keel to find the definition and references of IndexStats.
Use Keel to show callers of index_repository.
Use Keel to estimate the impact of changing LanguagePlugin.
```

## Local JSON API

Start the server from an indexed project:

```bash
keel serve --port 7645
```

Test it:

```bash
curl http://127.0.0.1:7645/health
curl http://127.0.0.1:7645/symbol/AuthService
```

It binds to `127.0.0.1`, so it is local-only by default.

## Troubleshooting

### `keel: command not found`

```bash
export PATH="$HOME/.cargo/bin:$PATH"
```

### Queries return no results

1. Confirm the shell and MCP server use the same project directory.
2. Run `keel index .` again.
3. Check whether `.gitignore` excludes the file.
4. Use the exact, case-sensitive symbol name.
5. Rebuild with `rm -rf .keel && keel index .`.

### MCP tools return empty results

Ensure the MCP configuration's `cwd` contains the expected
`.keel/index.db`.

### Build fails while compiling SQLite

Install a C compiler using the prerequisite instructions. On macOS:

```bash
xcode-select -p
```

## Uninstall

```bash
cargo uninstall keel
```

Remove any per-project indexes separately:

```bash
rm -rf /path/to/project/.keel
```

## Further documentation

See [`keel/README.md`](keel/README.md) for:

- Rust library API usage
- Community language plugins
- MCP protocol details
- JSON response schemas
- Resolution model and current limitations
- Full CLI reference

