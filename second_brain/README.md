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
sb index <path>            # index a repository into ./.secondbrain/index.db
sb definition <name>       # where a symbol is defined
sb references <name>       # where a name is referenced (call/macro sites in v0.1)
sb callers <name>          # call/use sites of a function (name-based in v0.1)
```

Output is `path:line:col` (1-based), tab-separated, stable and script-friendly.

## Scope (v0.1)

- Rust only; name-based resolution (same-named matches are all reported).
- Precise, scope-aware resolution via Stack Graphs is planned for v0.2.

## License

Open source (see repository).
