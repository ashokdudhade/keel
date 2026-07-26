# Keel

Deterministic, local-first code intelligence for AI coding agents. Keel
indexes source with Tree-sitter and answers structural queries from a local
SQLite database—no LLMs, embeddings, or semantic search.

**Languages:** Rust, TypeScript/TSX, JavaScript/JSX, Python, Go.

**Primary use:** Cursor (and other agents) via MCP. CLI and a local JSON API
are also available.

## Install

```bash
# macOS (Homebrew)
brew tap ashokdudhade/keel https://github.com/ashokdudhade/keel
brew install ashokdudhade/keel/keel

# macOS or Linux (curl)
curl -fsSL https://raw.githubusercontent.com/ashokdudhade/keel/main/install.sh | sh
```

Confirm:

```bash
keel --help
```

Curl installs to `~/.local/bin` (override with `KEEL_INSTALL_DIR`). Pin a
release with `KEEL_VERSION=v1.1.2`. Add that directory to `PATH` if needed.

Both installers need a published GitHub Release with binaries. If that fails,
build from source in [`keel/README.md`](keel/README.md#build-from-source-contributors).

> The crates.io name `keel` is taken. Use GitHub binaries / Homebrew, not
> `cargo install keel` from crates.io.

## Quick start (Cursor)

```bash
# 1. Once per machine
brew services start keel          # or: keel daemon

# 2. In each project you care about
cd /path/to/your/project
keel start                        # indexes into .keel/ and watches files
keel status                       # confirm daemon + watch
```

Add `.keel/` to the project's `.gitignore`.

### 3. Wire Cursor MCP

Use an **absolute** path to the binary and set `KEEL_INDEX_DB` to the project
index. Prefer `env` over `cwd`—Cursor often ignores `cwd`.

```bash
which keel
# e.g. /opt/homebrew/bin/keel  or  /usr/local/bin/keel  or  ~/.local/bin/keel
```

Global `~/.cursor/mcp.json` or project `.cursor/mcp.json`:

```json
{
  "mcpServers": {
    "keel": {
      "command": "/opt/homebrew/bin/keel",
      "args": ["mcp"],
      "env": {
        "KEEL_INDEX_DB": "/absolute/path/to/your/project/.keel/index.db"
      }
    }
  }
}
```

Replace both paths with yours. After saving, refresh MCP in Cursor Settings.
You should see **seven tools**: `definition`, `references`, `callers`,
`implementations`, `dependencies`, `impact`, `index`.

### 4. Try it

In chat:

```text
Use keel definition for AuthService
Use keel references for create_order
```

CLI works the same way without MCP:

```bash
keel definition AuthService
keel references create_order
keel callers create_order
```

Example output:

```text
src/auth/service.rs:12:12	struct	AuthService
```

## Everyday commands

```bash
brew services start keel          # global daemon (once per machine)
keel start [path]                 # register this project (index + watch)
keel stop                         # unregister this project only
keel status                       # daemon + this project
keel definition <name>            # find definitions (auto-indexes)
keel references <name>
keel callers <name>
keel implementations <trait>
keel dependencies <name|module>
keel impact <name>
```

Index path: `./.keel/index.db`. Daemon state: `~/.keel/daemon/` (`KEEL_HOME`).

Global flag: `--no-auto-index` skips the incremental ensure-index before queries.

## Without Homebrew / without the daemon

```bash
keel daemon                       # leave running (or use a supervisor)
# or one-shot / foreground:
keel index .
keel watch .
```

## Local JSON API (optional)

```bash
keel serve --port 7645
curl http://127.0.0.1:7645/health
curl http://127.0.0.1:7645/symbol/AuthService
```

Binds to `127.0.0.1` only by default.

## Troubleshooting

### `keel: command not found`

```bash
export PATH="${KEEL_INSTALL_DIR:-$HOME/.local/bin}:$PATH"
```

### `keel daemon is not running`

`keel start` needs the global daemon:

```bash
brew services start keel   # or: keel daemon
keel status
```

### MCP tools never appear / yellow “loading tools”

1. Use an **absolute** `command` path (`which keel`), not a bare `keel`.
2. Set `KEEL_INDEX_DB` to an existing `…/.keel/index.db` (run `keel start` first).
3. Do not rely on `cwd` alone.
4. Refresh MCP servers in Cursor Settings after editing `mcp.json`.
5. Smoke-test outside Cursor:

```bash
printf '%s\n' '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}' \
  | KEEL_INDEX_DB=/absolute/path/to/project/.keel/index.db "$(which keel)" mcp
```

You should get a single JSON line back (not hang).

### Queries or MCP tools return empty results

1. Confirm `KEEL_INDEX_DB` (or your shell cwd) points at the right project.
2. Run `keel index .` or `keel start` again.
3. Check `.gitignore` is not excluding the file you care about.
4. Use the exact, case-sensitive symbol name.
5. Rebuild: `rm -rf .keel && keel start` (daemon must be up).

## Uninstall

```bash
brew services stop keel
brew uninstall keel                                    # Homebrew
rm -f "${KEEL_INSTALL_DIR:-$HOME/.local/bin}/keel"     # curl
```

Per-project indexes: `rm -rf /path/to/project/.keel`.  
Optional daemon state: `rm -rf ~/.keel`.

## Accuracy

On popular GitHub repositories (walkdir, zod, express, flask, cobra) with
hand-verified gold symbols:

| Method | Precision | Recall | F1 |
|--------|-----------|--------|----|
| Without Keel (keyword grep) | 78.9% | 71.4% | 75.0% |
| With Keel | 100% | 100% | 100% |

Full report: [`reports/realworld-accuracy-benchmark.html`](reports/realworld-accuracy-benchmark.html).

## Further documentation

[`keel/README.md`](keel/README.md) — library API, language plugins, MCP/HTTP
contracts, resolution model, and contributor build notes.
