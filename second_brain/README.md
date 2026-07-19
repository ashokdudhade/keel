# SecondBrain

Deterministic, local-first code intelligence for AI coding agents. SecondBrain
indexes a Rust repository with Tree-sitter and answers structural queries from a
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
```

CLI output is `path:line:col` (1-based), tab-separated, stable and script-friendly.

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

## Resolution model (v0.2)

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

- Rust only in v0.2.
- Incremental indexing via content hashes; `sb watch` for live updates.
- Graph queries: implementations, dependencies, transitive impact.

## License

Open source (see repository).
