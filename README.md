# Keel

Deterministic, local-first code intelligence for AI coding agents. Keel
indexes source code with Tree-sitter and answers structural queries from a local
SQLite database—without LLMs, embeddings, or semantic search.

Supported languages:

- Rust (`.rs`)
- TypeScript / TSX (`.ts`, `.tsx`, `.mts`, `.cts`)
- JavaScript / JSX (`.js`, `.jsx`, `.mjs`, `.cjs`)
- Python / Python stubs (`.py`, `.pyi`)
- Go (`.go`)

Interfaces:

- `keel` command-line tool
- MCP stdio server for Cursor and other coding agents
- Local JSON HTTP API
- Stable Rust `Index` library API

## Install

Pick one:

```bash
# macOS (Homebrew) — formula lives in this repo; tap it first
brew tap ashokdudhade/keel https://github.com/ashokdudhade/keel
brew install ashokdudhade/keel/keel

# macOS or Linux (curl)
curl -fsSL https://raw.githubusercontent.com/ashokdudhade/keel/main/install.sh | sh
```

Both paths need a published GitHub Release with binaries. If install fails
looking for release archives, use a source build instead (see
[`keel/README.md`](keel/README.md#build-from-source-contributors)) or wait for
the next tagged release.

Then confirm:

```bash
keel --help
```

The curl installer puts `keel` in `~/.local/bin` (override with `KEEL_INSTALL_DIR`).
Pin a release with `KEEL_VERSION=v1.1.0`.

Homebrew does **not** edit `.zshrc` / `.bashrc`; it installs into Homebrew’s
`bin`, which is already on PATH when Homebrew itself is set up.

Building from source is only needed for Keel development — see
[`keel/README.md`](keel/README.md#build-from-source-contributors).

## Quick start

```bash
# Once per machine — Homebrew keeps the daemon running
brew services start keel
# Without Homebrew: keel daemon

# Per project — indexes into .keel/ and registers a file watcher
cd /path/to/your/project
keel start

# Queries auto-run a fast incremental index when needed
keel definition AuthService
keel references create_order
keel callers create_order

keel status   # global daemon + this project's watch
keel stop     # unregister this project only
```

Index path: `./.keel/index.db` (add `.keel/` to `.gitignore`).
Daemon state: `~/.keel/daemon/` (override with `KEEL_HOME`).

Example output:

```text
src/auth/service.rs:12:12	struct	AuthService
```

## Commands

```bash
brew services start keel          # start global daemon (Homebrew)
keel daemon [--port 7646]         # same daemon in the foreground
keel start [path]                 # register project: index + watch
keel stop                         # unregister this project
keel status                       # daemon + this project
keel index <path>                 # one-shot create/update the index
keel watch <path>                 # foreground re-index on file changes
keel definition <name>            # find definitions (auto-indexes)
keel references <name>            # find references (auto-indexes)
keel callers <name>               # find call/use sites (auto-indexes)
keel implementations <trait>      # find trait implementations
keel dependencies <name|module>   # find dependencies
keel impact <name>                # estimate transitive impact
keel serve [--port 7645]          # run the local JSON API
keel mcp                          # run the MCP stdio server
```

Global flag: `--no-auto-index` skips the incremental ensure-index before queries.

## Keep the index current

Preferred: one global daemon, then register each project you care about.

```bash
brew services start keel          # once per machine
cd /path/to/your/project && keel start
keel status
keel stop                         # when you no longer need that project watched
```

Without brew services, run the daemon yourself:

```bash
keel daemon                       # leave running in a terminal or supervisor
```

One-shot refresh / foreground watcher (no daemon):

```bash
keel index .
keel watch .
# reset: rm -rf .keel && keel index .
```

## Use with Cursor via MCP

```bash
brew services start keel
cd /path/to/your/project
keel start   # optional; queries also auto-index
which keel
```

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

`cwd` must be the indexed project so `keel mcp` opens `./.keel/index.db`.

Available MCP tools: `definition`, `references`, `callers`, `implementations`,
`dependencies`, `impact`, `index`.

## Local JSON API

```bash
keel serve --port 7645
curl http://127.0.0.1:7645/health
curl http://127.0.0.1:7645/symbol/AuthService
```

Binds to `127.0.0.1` only by default.

## Accuracy

On popular GitHub repositories (walkdir, zod, express, flask, cobra) with
hand-verified gold symbols:

| Method | Precision | Recall | F1 |
|--------|-----------|--------|----|
| Without Keel (keyword grep) | 78.9% | 71.4% | 75.0% |
| With Keel | 100% | 100% | 100% |

Full report: [`reports/realworld-accuracy-benchmark.html`](reports/realworld-accuracy-benchmark.html)
(regenerate with `scripts/realworld-accuracy-benchmark.sh`).

## Troubleshooting

### `keel: command not found`

```bash
# curl install
export PATH="${KEEL_INSTALL_DIR:-$HOME/.local/bin}:$PATH"
```

Homebrew usually configures PATH during install.

### `keel daemon is not running`

`keel start` needs the global daemon:

```bash
brew services start keel
# or: keel daemon
keel status
```

### Queries return no results

1. Confirm the shell and MCP server use the same project directory.
2. Run `keel index .` (or `keel start` with the daemon up) again.
3. Check whether `.gitignore` excludes the file.
4. Use the exact, case-sensitive symbol name.
5. Rebuild with `rm -rf .keel && keel index .`.

### MCP tools return empty results

Ensure the MCP configuration's `cwd` contains the expected `.keel/index.db`.

## Uninstall

```bash
brew services stop keel                                # if started via brew
brew uninstall keel                                    # Homebrew
rm -f "${KEEL_INSTALL_DIR:-$HOME/.local/bin}/keel"     # curl
```

Remove per-project indexes with `rm -rf /path/to/project/.keel`.
Optional daemon state: `rm -rf ~/.keel`.

## Further documentation

See [`keel/README.md`](keel/README.md) for the library API, plugins, MCP/HTTP
details, resolution model, and CLI reference.

> The crates.io name `keel` is already taken. Public installs use GitHub
> release binaries (curl / Homebrew), not `cargo install keel` from crates.io.
