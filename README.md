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
# macOS (Homebrew)
brew install --formula \
  https://raw.githubusercontent.com/ashokdudhade/keel/main/Formula/keel.rb

# macOS or Linux (curl)
curl -fsSL https://raw.githubusercontent.com/ashokdudhade/keel/main/install.sh | sh
```

Then confirm:

```bash
keel --help
```

The curl installer puts `keel` in `~/.local/bin` (override with `KEEL_INSTALL_DIR`).
Pin a release with `KEEL_VERSION=v1.1.0`.

Building from source is only needed for Keel development — see
[`keel/README.md`](keel/README.md#build-from-source-contributors).

## Quick start

```bash
cd /path/to/your/project

keel index .
keel definition AuthService
keel references create_order
keel callers create_order
```

Index path: `./.keel/index.db` (add `.keel/` to `.gitignore`).

Example output:

```text
src/auth/service.rs:12:12	struct	AuthService
```

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

## Keep the index current

```bash
keel watch .
# or: keel index .
# reset: rm -rf .keel && keel index .
```

## Use with Cursor via MCP

```bash
cd /path/to/your/project
keel index .
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

### Queries return no results

1. Confirm the shell and MCP server use the same project directory.
2. Run `keel index .` again.
3. Check whether `.gitignore` excludes the file.
4. Use the exact, case-sensitive symbol name.
5. Rebuild with `rm -rf .keel && keel index .`.

### MCP tools return empty results

Ensure the MCP configuration's `cwd` contains the expected `.keel/index.db`.

## Uninstall

```bash
brew uninstall keel                                    # Homebrew
rm -f "${KEEL_INSTALL_DIR:-$HOME/.local/bin}/keel"     # curl
```

Remove per-project indexes with `rm -rf /path/to/project/.keel`.

## Further documentation

See [`keel/README.md`](keel/README.md) for the library API, plugins, MCP/HTTP
details, resolution model, and CLI reference.

> The crates.io name `keel` is already taken. Public installs use GitHub
> release binaries (curl / Homebrew), not `cargo install keel` from crates.io.
