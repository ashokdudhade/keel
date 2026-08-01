# Keel

Local code intelligence for AI coding agents. Keel indexes your repository
with Tree-sitter and answers structural queries from a local on-disk
index—no LLMs, embeddings, or cloud index.

**What it is:** name-based structural search (not semantic search). Grep
finds text; language servers resolve types in an IDE. Keel is a persistent
symbol index your agent queries by name. For a given index state, answers
are stable and repeatable.

**Languages:** Rust, TypeScript/TSX, JavaScript/JSX, Python, Go (mixed
monorepos in one pass).

**Interfaces:** MCP (`keel mcp`) for Cursor, Claude Code, and other MCP
clients; CLI for the same queries; optional JSON API on `127.0.0.1`.

**Site:** [ashokdudhade.github.io/keel](https://ashokdudhade.github.io/keel/)
(`website/`).

## Install

Install → daemon → index → MCP. Pick one path:

| Platform | Recommended |
|----------|-------------|
| macOS with Homebrew | Homebrew (PATH + `brew services`) |
| macOS without Homebrew, or Linux | curl installer (detects OS/arch, fixes PATH) |
| Windows | WSL2, then the curl installer |

### Homebrew (macOS)

```bash
brew tap ashokdudhade/keel https://github.com/ashokdudhade/keel
brew install ashokdudhade/keel/keel
keel --help
```

### curl (macOS / Linux / WSL2)

```bash
curl -fsSL https://raw.githubusercontent.com/ashokdudhade/keel/main/install.sh | sh
```

The script:

1. Detects OS and CPU (`macOS` / `Linux`, `arm64` / `x86_64`)
2. Downloads the matching GitHub Release binary (SHA-256 verified)
3. Installs to `~/.local/bin/keel` (override with `KEEL_INSTALL_DIR`)
4. Adds that directory to PATH in `~/.profile` and your shell rc (`zsh` /
   `bash` / `fish`) so `keel` is available in **new** terminals

Then:

```bash
# open a new terminal, or: source ~/.zshrc
keel --help
```

Useful env vars:

| Variable | Purpose |
|----------|---------|
| `KEEL_VERSION` | Pin a tag, e.g. `v1.1.2` (default: latest) |
| `KEEL_INSTALL_DIR` | Install directory (default: `~/.local/bin`) |
| `KEEL_NO_MODIFY_PATH` | Set to `1` to skip editing shell profiles |

Native Windows is not supported. Use WSL2 and run the curl installer there.

Both installers need a published GitHub Release with binaries. If install
fails looking for archives, build from source in
[`keel/README.md`](keel/README.md#build-from-source-contributors).

> The crates.io name `keel` is taken. Use GitHub binaries / Homebrew, not
> `cargo install keel` from crates.io.

## Quick start (Cursor)

### 1. Start the global daemon (once per machine)

```bash
# Homebrew install:
brew services start keel

# curl install (or any non-Brew setup):
keel daemon          # leave running in a terminal or process supervisor
```

### 2. Register your project

```bash
cd /path/to/your/project
keel start           # indexes into .keel/ and watches files
keel status          # confirm daemon + watch
```

Add `.keel/` to the project's `.gitignore`.

### 3. Wire Cursor MCP

Use an **absolute** path to the binary and set `KEEL_INDEX_DB` to the project
index. Prefer `env` over `cwd`—Cursor often ignores `cwd`.

```bash
which keel
# Homebrew examples: /opt/homebrew/bin/keel  or  /usr/local/bin/keel
# curl default:      ~/.local/bin/keel  → expand to a full path
```

Global `~/.cursor/mcp.json` or project `.cursor/mcp.json`:

```json
{
  "mcpServers": {
    "keel": {
      "command": "/absolute/path/to/keel",
      "args": ["mcp"],
      "env": {
        "KEEL_INDEX_DB": "/absolute/path/to/your/project/.keel/index.db"
      }
    }
  }
}
```

Replace both paths with yours. After saving, refresh MCP in Cursor Settings.
You should see **seven tools**:

| Tool | Purpose |
|------|---------|
| `definition` | Definition location(s) for a symbol name |
| `references` | Reference sites for a name |
| `callers` | Call/use sites; import-aware when a unique definition module is known |
| `implementations` | Rust trait implementations for a trait name |
| `dependencies` | Modules/files a module or symbol depends on |
| `impact` | Symbols transitively impacted by changing a name |
| `index` | Index a repository path; returns indexing stats |

Prefer Keel when you know a symbol or trait name; use text search for regex.
The same `mcpServers` shape works for Claude Code and other MCP clients.

### 4. Prefer Keel automatically in chat

Add a project rule so Cursor uses Keel for structural search without you
saying “use keel” (this repo includes
[`.cursor/rules/keel-mcp.mdc`](.cursor/rules/keel-mcp.mdc)):

```text
.cursor/rules/keel-mcp.mdc
```

### 5. Try it

In chat:

```text
Where is AuthService defined?
Who references create_order?
Who calls create_order?
What is impacted if WireFormat changes?
```

Or from the CLI:

```bash
keel definition AuthService
keel references create_order
keel callers create_order
keel impact WireFormat
```

Example output:

```text
src/auth/service.rs:12:12	struct	AuthService
```

## Everyday commands

```bash
brew services start keel   # global daemon (Homebrew)
keel daemon                # global daemon (curl / foreground)
keel start [path]          # register this project (index + watch)
keel stop                  # unregister this project only
keel status                # daemon + this project
keel definition <name>     # find definitions (auto-indexes)
keel references <name>
keel callers <name>
keel implementations <trait>   # Rust traits today
keel dependencies <name|module>
keel impact <name>
```

Index path: `./.keel/index.db` (add `.keel/` to `.gitignore`). Daemon state:
`~/.keel/daemon/` (`KEEL_HOME`).

Global flag: `--no-auto-index` skips the incremental ensure-index before queries.

## Without the daemon

```bash
keel index .    # one-shot index
keel watch .    # foreground re-index on file changes
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

After the **curl** installer, open a **new terminal** (profiles were updated),
or for the current session:

```bash
export PATH="${KEEL_INSTALL_DIR:-$HOME/.local/bin}:$PATH"
```

Homebrew usually configures PATH during install.

### `keel daemon is not running`

`keel start` needs the global daemon:

```bash
brew services start keel   # Homebrew
# or:
keel daemon                # curl / foreground
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
brew services stop keel                                # if used
brew uninstall keel                                    # Homebrew
rm -f "${KEEL_INSTALL_DIR:-$HOME/.local/bin}/keel"     # curl
```

Optional: remove the PATH block labeled `keel PATH (managed by install.sh)`
from `~/.profile` and your shell rc.

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
