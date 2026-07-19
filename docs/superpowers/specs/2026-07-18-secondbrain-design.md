# Keel — Design Specification

**Date:** 2026-07-18
**Status:** Approved for v0.1 implementation
**Source docs:** `Keel_Proposal.md`, `keel_cursorrules.md`

---

## 1. Overview

Keel is an open-source, local-first **code intelligence engine** written in
Rust. It builds the most accurate, language-aware representation of a software
repository and answers structural questions about it **deterministically**.

It is **infrastructure, not an agent**. It does not generate code, rewrite pull
requests, fix CI, or replace coding assistants. It provides trusted, deterministic
context that any AI coding agent (Cursor, Claude Code, Codex CLI, Continue,
OpenHands, …) can consume via a library, CLI, JSON API, or MCP server.

### Hard constraint (non-negotiable)

No LLMs, no embeddings, no semantic/vector search anywhere in symbol resolution.
Everything is derived from Abstract Syntax Trees (Tree-sitter) and explicit graph
relationships (Stack Graphs). Given the same repository state, every query returns
the same answer.

### Questions Keel answers

- Where is this symbol defined? (`definition`)
- Where is this symbol referenced? (`references`)
- Who calls this function? (`callers`)
- What implements this trait/interface? (`implementations`) — v0.2+
- What depends on this module? (`dependencies`) — v0.2+
- What breaks if I change this? (`impact`) — v0.2+

---

## 2. Goals & Non-Goals

### Goals

- **Local-first:** runs entirely on the developer's machine, no network required.
- **Deterministic:** identical repo state → identical results, always.
- **Incremental:** only reprocess changed files (v0.2+).
- **Language-agnostic core:** language specifics live behind a trait; the core never
  hardcodes Rust (or any language) rules.
- **Low footprint & fast startup:** suitable for large monorepos (>1M LOC target).
- **Multiple interfaces:** Rust library, CLI, JSON API, MCP server.

### Non-Goals

Keel will **not**: generate code, rewrite PRs, fix CI failures, create ADRs,
or replace coding assistants. It will not use machine learning for resolution.

---

## 3. Architecture

```text
        AI coding agents (Cursor, Claude Code, Codex, Continue, OpenHands)
                                   │
                    Library API │ CLI │ JSON API │ MCP
                                   │
                        ┌──────────────────────┐
                        │   Keel Core    │
                        │  (language-agnostic)  │
                        └──────────────────────┘
              index/         graph/          db/           languages/
          crawl + parallel   domain types   SQLite store   LanguagePlugin trait
          worker pipeline    + query surface + queries      (rust, ts, go, …)
                                   │
                 Tree-sitter  │  Stack Graphs  │  SQLite  │  Rayon  │  notify
```

### Two-layer resolution model

Resolution is layered so there is always a working baseline and a precise upgrade:

1. **Name index (baseline — v0.1).** Every symbol and reference is stored in SQLite
   keyed by name. Lookups match by name. Fast, simple, always available. On its own
   this can return multiple same-named candidates, which the CLI reports as ranked
   candidates.
2. **Stack Graphs (precise — v0.2).** `tree-sitter-stack-graphs` builds scope-aware
   name-binding graphs from the parse tree. Resolution walks partial paths in the
   stack graph so `definition`/`references`/`callers` resolve to the *correct* symbol
   across files, disambiguating shadowing, imports, and scoping. When present it is
   authoritative; the name index remains the fallback and the substrate for
   `kind`/location metadata.

> **v0.1 scope decision.** The stack-graphs *framework* is published, but there is
> no maintained published Rust rule crate (`tree-sitter-stack-graphs-rust` is not on
> crates.io; only Python/Java/TypeScript rules are). Authoring complete Rust
> name-binding rules is a substantial, research-grade effort. Therefore **v0.1 ships
> the name-index layer only**, and the Stack Graphs precision layer is a dedicated
> **v0.2** effort with its own plan. The architecture below keeps the plugin trait
> and storage ready for that layer without blocking v0.1.

---

## 4. Component Design

Modular crate layout (the core stays language-agnostic; Rust logic is isolated
behind `LanguagePlugin`).

```text
keel/
├── Cargo.toml
├── README.md
├── src/
│   ├── lib.rs              # Public library API exports
│   ├── main.rs             # CLI entry point
│   ├── error.rs            # Core error types (thiserror)
│   ├── db/
│   │   ├── mod.rs          # DB init and connection management
│   │   ├── schema.rs       # SQLite tables + migrations
│   │   └── queries.rs      # Type-safe insert/select wrappers
│   ├── index/
│   │   ├── mod.rs          # Index orchestration
│   │   └── worker.rs       # Rayon parallel processing pipeline
│   ├── graph/
│   │   ├── mod.rs          # Graph query surface
│   │   └── types.rs        # Symbol, Reference, FileNode types
│   ├── languages/
│   │   ├── mod.rs          # LanguagePlugin trait + registry
│   │   └── rust.rs         # Tree-sitter queries + stack-graph rules
│   └── cli/
│       ├── mod.rs          # clap struct definitions
│       └── commands.rs     # Command execution logic
└── tests/                  # Integration tests + fixture repos
```

### 4.1 `graph/types.rs` — domain types

Strongly-typed domain concepts. Paths are `PathBuf`, never `String`.

- `Symbol { name: String, kind: SymbolKind, file: PathBuf, start_line: u32, start_col: u32 }`
- `Reference { name: String, file: PathBuf, start_line: u32, start_col: u32 }`
- `FileNode { path: PathBuf, content_hash: String }`
- `SymbolKind` enum: `Function`, `Struct`, `Trait`, `Enum`, `Impl`, `Module`,
  `Const`, `Other(String)` — extensible for future languages.

### 4.2 `db/` — SQLite storage

Uses `rusqlite` with the **bundled** SQLite feature (no system dependency).
Read-heavy design; the schema is optimized for name lookups.

Tables:

- `files(id INTEGER PK, path TEXT UNIQUE NOT NULL, content_hash TEXT NOT NULL)`
- `symbols(id INTEGER PK, file_id INTEGER FK→files, name TEXT NOT NULL, kind TEXT NOT NULL, start_line INTEGER, start_col INTEGER)`
- `references(id INTEGER PK, file_id INTEGER FK→files, name TEXT NOT NULL, start_line INTEGER, start_col INTEGER)`

Indexes: `idx_symbols_name` on `symbols(name)`, `idx_references_name` on
`references(name)`.

In v0.2, stack-graph data uses the `stack-graphs` SQLite storage backend (its own
tables / db handle) for cross-file resolution; v0.1 does not create these tables.

`content_hash` is populated from v0.1 but only *consumed* for incremental updates in
v0.2. Schema changes go through a simple, versioned migration path.

### 4.3 `languages/` — plugin system

```rust
pub trait LanguagePlugin {
    /// File extensions this plugin handles (e.g., ["rs"]).
    fn extensions(&self) -> &[&str];
    fn extract_symbols(&self, source_code: &str) -> Result<Vec<Symbol>, KeelError>;
    fn extract_references(&self, source_code: &str) -> Result<Vec<Reference>, KeelError>;
    // v0.2: fn build_stack_graph(&self, ctx: &mut StackGraphCtx, source_code: &str)
    //       -> Result<(), KeelError>;  // scope-aware resolution
}
```

`rust.rs` implements this with Tree-sitter S-expression queries (functions,
structs, traits, enums, impls, and their references). A registry maps file
extensions to plugins, so the core dispatches without knowing language specifics.
The stack-graph rule method is added to the trait in v0.2 when the precision layer
lands.

### 4.4 `index/` — indexing engine

- Crawl the target directory with `ignore`/`walkdir`, respecting `.gitignore`.
- Route each file to the plugin registered for its extension.
- Pipeline per file: read → parse (Tree-sitter) → extract symbols/references →
  build stack graph → batch-insert into SQLite.
- Parallelize across files with `rayon`. Writes are batched; SQLite access is
  serialized where required for correctness.

### 4.5 `cli/` — command-line interface

`clap` (derive). Binary name `keel`, crate `keel`.

- `keel index <path>` — index a repository.
- `keel definition <name>` — print definition location(s).
- `keel references <name>` — print reference locations.
- `keel callers <name>` — print call sites for a function.

Output is clean, stable `path:line:col`-style text on stdout, easily parsed by
humans and agents. Diagnostics go to stderr.

### 4.6 `error.rs` — errors

`thiserror`-based `KeelError` for the library (I/O, parse, DB, plugin,
resolution variants). The binary boundary (`main.rs`/CLI) uses `anyhow` for
context-rich top-level error reporting.

---

## 5. Data Flow

**Indexing (`keel index <path>`):**
crawl files → for each file in parallel: parse → extract symbols+references →
batch insert into SQLite (`files`/`symbols`/`references`). (v0.2 adds building the
stack graph and persisting to the stack-graph store.)

**Querying (`keel definition|references|callers <name>`):**
load candidates from SQLite by name → format and print `path:line:col`. Multiple
same-named matches are all reported. (v0.2 adds precise stack-graph resolution ahead
of the name-index fallback.) `callers` returns reference sites of a function name.

---

## 6. Coding Standards

- **No panics:** no `.unwrap()`/`.expect()` outside tests; propagate with `?`.
- **No `unsafe`.**
- **Strong types:** domain concepts use real types (`PathBuf`, `SymbolKind`), not
  stringly-typed data.
- **`thiserror`** in library code; **`anyhow`** only at the CLI boundary.
- **Rustdoc** (`///`) on all public modules, traits, and functions; document *why*
  a Tree-sitter query or stack-graph rule is written a certain way.
- **Modularity:** `db/` and `index/` never hardcode language specifics; all
  language logic stays behind `LanguagePlugin`.
- **Edition 2021.**

---

## 7. Testing Strategy

TDD throughout — write the failing test first.

- **DB tests:** in-memory SQLite (`rusqlite::Connection::open_in_memory()`) to verify
  schema creation, inserts, and name lookups.
- **Extraction tests:** dummy Rust source strings assert extracted symbol names,
  kinds, and line/column positions.
- **Resolution tests:** small multi-file fixture repos assert that `definition`/
  `references`/`callers` resolve to the correct cross-file symbol (including
  shadowing/import disambiguation).
- **CLI / end-to-end tests:** index a fixture repo, run each command, assert on
  stdout format and content.

---

## 8. Roadmap

The spec covers the full roadmap; each version beyond v0.1 gets its own
implementation plan when reached.

### v0.1 — Foundation (shipped)

- Cargo scaffolding, error types, domain types.
- SQLite storage layer (schema, migrations, type-safe queries).
- `LanguagePlugin` trait + Rust plugin (Tree-sitter extraction).
- Name-index resolution (symbols/references matched by name).
- Parallel indexing engine (`rayon`, `ignore`, `walkdir`).
- CLI: `index`, `definition`, `references`, `callers`.

### v0.2 — Precise resolution, incremental & richer graph (shipped)

- **In-house deterministic module/import-aware resolver** (Stack Graphs deferred;
  no published Rust `.tsg` crate — see decision in v0.2 plan).
- Incremental indexing via `content_hash` diffing + `notify` file watching (`keel watch`).
- Dependency graph and `impact` analysis.
- `implementations` and `dependencies` queries.
- JSON API (`keel serve` — `GET /symbol/<name>`, `GET /health`).

### v0.3 — More languages & MCP (shipped)

- MCP server (`keel mcp` — stdio JSON-RPC tools over the same core).
- TypeScript/TSX plugin, Go plugin.

### v1.0 — Multi-language & stability (shipped 2026-07-19)

- Multi-language monorepo support.
- Full impact analysis.
- Plugin system for community language contributions (`Registry::register`).
- Stable public APIs (`Index` facade) + documentation.
- Crate version `1.0.0`; see `keel/CHANGELOG.md`.

---

## 9. Technical Stack

| Component            | Technology                                   |
| -------------------- | -------------------------------------------- |
| Language             | Rust (Edition 2021)                          |
| Parsing              | `tree-sitter`, `tree-sitter-rust`            |
| Resolution (v0.1)    | Name index in SQLite                         |
| Resolution (v0.2+)   | Module/import-aware deterministic resolver (Stack Graphs optional future) |
| Languages            | Rust, TypeScript/TSX, Go                                 |
| Agent interfaces     | CLI (`keel`), JSON HTTP (`keel serve`), MCP (`keel mcp`)       |
| Storage              | `rusqlite` (bundled SQLite)                  |
| Parallelism          | `rayon`                                      |
| CLI                  | `clap` (derive)                              |
| Errors               | `thiserror` (lib), `anyhow` (binary)         |
| File traversal       | `ignore`, `walkdir`                          |
| File watching (v0.2) | `notify`                                     |

---

## 10. Success Metrics

**Technical**

- Incremental indexing under 500 ms for typical file changes (v0.2).
- Supports repositories >1M LOC.
- Accurate cross-file symbol resolution (deterministic).
- Low memory footprint.

**Adoption**

- Integration with major AI coding assistants.
- Community-contributed language plugins.
- Reused as a library by open-source developer tools.

---

## 11. Open Questions / Deferred Decisions

- **Stack Graphs Rust rules (v0.2):** no maintained published crate exists; v0.2 must
  author/vendor the Rust `.tsg` name-binding rules and pin compatible `tree-sitter`
  versions (`tree-sitter-stack-graphs` currently requires `tree-sitter ^0.24`).
- **Stack-graph storage layout (v0.2):** single SQLite file with separate table
  namespaces vs. a separate `.db` — default to a single file; settle during v0.2.
- **`callers` semantics (v0.1):** name-based — reference sites whose name matches the
  target function. This can over-report same-named functions until the v0.2
  precision layer disambiguates.
