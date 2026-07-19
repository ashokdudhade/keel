# SecondBrain v0.2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extend SecondBrain from a name index into a resolved cross-file symbol graph with incremental indexing, dependency/impact analysis, and a JSON API — all deterministic, no LLMs/embeddings.

**Architecture:** Build on the v0.1 crate. Add a schema migration runner, richer Rust extraction (module paths, `use` imports, `impl` trait relationships, method calls), an in-house **deterministic AST resolver** (module-path + import aware) as the precision layer (chosen over the unpublished `tree-sitter-stack-graphs-rust`), incremental indexing (hash-diff + stale cleanup + `sb watch`), graph queries (`implementations`/`dependencies`/`impact`), and a `sb serve` JSON API.

**Tech Stack:** As v0.1, plus `notify` (file watching), `tiny_http` (JSON API server), `serde` + `serde_json` (JSON).

## Global Constraints

- Rust edition 2021. Crate `second_brain`, binary `sb`.
- No `.unwrap()`/`.expect()` outside `#[cfg(test)]`. No `unsafe`. `PathBuf` for paths.
- `thiserror` in the library; `anyhow` only at the `main.rs` boundary.
- Rustdoc on every public module/trait/function. 1-based positions.
- The SQL word `references` is always quoted as `"references"`.
- Determinism: identical repo state → identical query output; all returned collections `ORDER BY` a stable key (typically `(path, start_line, start_col)`).
- No LLMs, embeddings, or semantic search. Resolution is AST + explicit graph relationships only.
- Every task keeps `cargo test` and `cargo clippy --all-targets -- -D warnings` green; commit at the end of each task.
- Environment: toolchain stable Rust 1.97.1; if a `cargo` command fails with a `cc`/Xcode-license error, prefix with `DEVELOPER_DIR=/Library/Developer/CommandLineTools`; release builds may need to run unsandboxed.
- Backward compatibility: opening a v0.1 database must transparently migrate it to v2 (no data loss for `files`; symbols/references get sensible defaults until re-index).

---

## Data Model (v2)

Add a migration runner keyed on `PRAGMA user_version` (v1 = the v0.1 tables; v2 = below).

- `symbols`: add `module_path TEXT NOT NULL DEFAULT ''` (e.g. `crate::auth::service`). Index `idx_symbols_module` on `(module_path, name)`.
- `"references"`: add `kind TEXT NOT NULL DEFAULT 'call'` (`call|macro|method|type|path`) and `container TEXT NOT NULL DEFAULT ''` (qualified name of the enclosing function/module).
- New `imports (id INTEGER PK, file_id INTEGER NOT NULL REFERENCES files(id), module_path TEXT NOT NULL, alias TEXT)`. Index on `module_path`.
- New `impls (id INTEGER PK, file_id INTEGER NOT NULL REFERENCES files(id), type_name TEXT NOT NULL, trait_name TEXT, start_line INTEGER NOT NULL, start_col INTEGER NOT NULL)`. Index on `trait_name` and `type_name`.
- `clear_file_rows` must also delete the file's `imports` and `impls` rows.

## Domain Types (v2)

In `graph/types.rs`:
- `Symbol` gains `pub module_path: String` (empty allowed).
- New `ReferenceKind` enum: `Call, Macro, Method, Type, Path`, with `as_db`/`from_db` like `SymbolKind`.
- `Reference` gains `pub kind: ReferenceKind` and `pub container: String`.
- New `ImplRecord { type_name: String, trait_name: Option<String>, file: PathBuf, start_line: u32, start_col: u32 }`.
- New `Import { module_path: String, alias: Option<String>, file: PathBuf }`.
- New `IndexStats { indexed: usize, skipped: usize, removed: usize }` (in `index`).

Each task updates ALL construction sites (extraction, queries, tests) so the build stays green.

---

## Task 1: Schema v2 + Migration Runner

**Files:** Modify `src/db/schema.rs`, `src/db/queries.rs`, `src/graph/types.rs`; tests in each.

**Interfaces produced:**
- `schema::initialize(&Connection)` now runs migrations to `user_version = 2` (idempotent; upgrades a v1 db in place).
- `Symbol.module_path: String`; `Reference.kind: ReferenceKind`, `Reference.container: String`; `ReferenceKind` enum with `as_db`/`from_db`.
- `queries::{insert_imports, insert_impls, clear_file_rows(extended), find_implementations}` scaffolding (implementations query lands here or T5 — see T5).

- [ ] Step 1 (TDD): write a test that `initialize` on a fresh in-memory db sets `PRAGMA user_version == 2` and that all tables/columns exist (`PRAGMA table_info(symbols)` includes `module_path`; `imports`/`impls` exist). Also a test that a simulated v1 db (create only the v1 tables + `user_version=1`) is upgraded to v2 with `files` data preserved.
- [ ] Step 2: run → fails.
- [ ] Step 3: implement a migration runner in `schema.rs`: read `user_version`; if 0 (fresh) create all v2 tables and set version 2; if 1, `ALTER TABLE` to add the new columns and `CREATE TABLE` the new tables, then set version 2. Keep `references` quoted. Add the new indexes.
- [ ] Step 4: extend `graph/types.rs` with `module_path` on `Symbol`, `ReferenceKind`, and `kind`/`container` on `Reference` (with rustdoc); update `insert_symbols`/`insert_references`/`find_definition`/`find_references` to persist/read the new columns; add `insert_imports`, `insert_impls`; extend `clear_file_rows` to also clear `imports`/`impls`. Update all existing tests/constructors to include the new fields (default `module_path: String::new()`, `kind: ReferenceKind::Call`, `container: String::new()`).
- [ ] Step 5: run full suite + clippy → green.
- [ ] Step 6: commit `feat(db): schema v2 with migrations, imports/impls tables, richer symbol/reference columns`.

---

## Task 2: Richer Rust Extraction

**Files:** Modify `src/languages/mod.rs` (extend `LanguagePlugin`), `src/languages/rust.rs`; tests inline.

**Interfaces produced (extend `LanguagePlugin`):**
- `fn extract_symbols` now sets `module_path` (from enclosing `mod` nesting; top level = `crate`).
- `fn extract_references` now sets `kind` (call/macro/method/type/path) and `container` (qualified name of the enclosing `fn`/`mod`, else empty), and additionally captures method calls (`field_expression`/`(call_expression function: (field_expression field: (field_identifier)))`), type references (`type_identifier` in type position), and path segments.
- New trait methods: `fn extract_imports(&self, src: &str) -> Result<Vec<Import>>` (from `use_declaration`, expanding `use a::{b, c}` into `a::b`, `a::c`, honoring `as` aliases) and `fn extract_impls(&self, src: &str) -> Result<Vec<ImplRecord>>` (from `impl_item`: `type_name`, optional `trait_name` when `impl Trait for Type`). Provide default impls returning `Ok(vec![])` on the trait so other plugins need not implement them yet.

- [ ] Step 1 (TDD): tests over a multi-item fixture asserting: a symbol inside `mod auth { fn login() }` has `module_path == "crate::auth"`; a `use std::collections::HashMap;` yields an `Import{ module_path: "std::collections::HashMap", .. }`; `use a::{b, c as d}` yields two imports with alias on the second; `impl Storage for MemStore {}` yields `ImplRecord{ type_name:"MemStore", trait_name:Some("Storage") }`; a method call `x.foo()` yields a `Reference{ name:"foo", kind:Method }`; a reference's `container` equals the enclosing function's qualified name.
- [ ] Step 2: run → fails.
- [ ] Step 3: implement. Walk the tree tracking the enclosing `mod` stack (for `module_path`) and enclosing `fn`/`mod` (for `container`). Add tree-sitter queries for `use_declaration`, `impl_item` (with `trait:` field), `field_expression`/method call, and type positions. Keep `parse` reused.
- [ ] Step 4: register the new extraction outputs in the indexer pipeline (`index/worker.rs` `ParsedFile` gains `imports: Vec<Import>` and `impls: Vec<ImplRecord>`; `parse_file` populates them) and persist them in `index/mod.rs` (`insert_imports`, `insert_impls`).
- [ ] Step 5: full suite + clippy → green.
- [ ] Step 6: commit `feat(lang): extract module paths, use imports, impl traits, method/type refs`.

---

## Task 3: Incremental Indexing + `sb watch`

**Files:** Modify `src/index/mod.rs`, `src/index/worker.rs`, `src/cli/*`, `src/main.rs`; add `src/index/watch.rs`; integration tests.

**Interfaces produced:**
- `index::index_repository` returns `IndexStats { indexed, skipped, removed }` (breaking change — update callers/tests). It: loads existing `path -> content_hash`; hashes candidate files first (cheap read) or hashes during parse; skips files whose hash is unchanged; parses+persists changed/new files; deletes DB rows for files no longer on disk (file row + symbols/refs/imports/impls). Determinism preserved.
- `index::watch::watch_repository(root, conn)` using `notify` — re-indexes on file events (debounced), runs until interrupted.
- CLI: `sb watch <path>`.

- [ ] Step 1 (TDD): integration tests — (a) index a fixture twice without changes → second run `skipped == indexed_count`, `indexed == 0`; (b) modify a file → only that file re-indexed (`indexed == 1`), query reflects the change; (c) delete a file from disk and re-index → its symbols are gone and `removed == 1`.
- [ ] Step 2: run → fails.
- [ ] Step 3: implement incremental logic. Add `queries::existing_hashes(&Connection) -> Result<HashMap<String,String>>` and `queries::delete_file_and_rows(&Connection, path) -> Result<()>`. Hash files up front (parallel) to decide skip vs parse.
- [ ] Step 4: implement `watch.rs` (notify recommended-watcher, debounce ~200ms, re-run `index_repository` on changes) and wire `sb watch`.
- [ ] Step 5: full suite + clippy → green. (Watch is validated by a short unit/integration test that triggers one debounce cycle, or documented as manually verified if a timing test is flaky — prefer a deterministic test that calls the debounced handler directly.)
- [ ] Step 6: commit `feat(index): incremental indexing with stale cleanup and sb watch`.

---

## Task 4: Deterministic Cross-File Resolver

**Files:** Add `src/graph/resolve.rs`; modify `src/graph/mod.rs`, `src/db/queries.rs`; tests.

**Interfaces produced:**
- `graph::resolve::resolve_definition(conn, name, from_container_or_module) -> Result<Vec<Symbol>>`: returns the best-matching definition(s) using deterministic rules: (1) exact `module_path::name` match reachable via an `imports` row in the caller's file; (2) same-module match; (3) fall back to all name matches (v0.1 behavior). Order matches by rule tier then `(path,line,col)`.
- `queries::find_definition_by_qualified(conn, module_path, name)` helper.
- Enhance `find_references`/callers to optionally filter by resolved target (still deterministic).

- [ ] Step 1 (TDD): fixtures with two same-named symbols in different modules; assert that resolving from a file that `use`s one of them returns that one first (tier 1), and that with no import the same-module one wins, and with neither, both are returned (stable order). No panics; pure SQL + deterministic ranking.
- [ ] Step 2: run → fails.
- [ ] Step 3: implement resolver as pure functions over the DB (module_path + imports). No ML, no heuristics beyond the documented tiers.
- [ ] Step 4: wire resolver into `find_callers` precision (callers of a function resolve to the specific definition when possible).
- [ ] Step 5: full suite + clippy → green.
- [ ] Step 6: commit `feat(graph): deterministic module/import-aware resolver`.

---

## Task 5: `find_implementations`

**Files:** Modify `src/db/queries.rs`, `src/cli/*`, `src/main.rs`; tests.

**Interfaces produced:**
- `queries::find_implementations(conn, trait_name) -> Result<Vec<ImplRecord>>` (ordered by `(path,line,col)`).
- CLI `sb implementations <trait>` printing `path:line:col\ttype_name` lines.

- [ ] Step 1 (TDD): index a fixture with `trait Storage` and two `impl Storage for A`/`impl Storage for B`; assert `find_implementations("Storage")` returns both, ordered; assert inherent `impl A {}` (no trait) is excluded.
- [ ] Step 2: run → fails.
- [ ] Step 3: implement query + CLI command + output.
- [ ] Step 4: full suite + clippy → green.
- [ ] Step 5: commit `feat: add find_implementations query and sb implementations`.

---

## Task 6: Dependency Graph + `find_dependencies`

**Files:** Add `src/graph/deps.rs`; modify `src/graph/mod.rs`, `src/db/queries.rs`, `src/cli/*`, `src/main.rs`; tests.

**Interfaces produced:**
- `graph::deps::find_dependencies(conn, target) -> Result<Vec<Dependency>>` where `target` is a module path or symbol name. A dependency is a module/file the target depends on, derived from `imports` (and references resolving to other files). `Dependency { module_path: String, file: Option<PathBuf> }`.
- CLI `sb dependencies <name>`.

- [ ] Step 1 (TDD): fixture where `mod a` imports `crate::b` and calls `b::f()`; assert `find_dependencies("crate::a")` includes `crate::b`; assert a leaf module with no imports returns empty; results deterministically ordered and de-duplicated.
- [ ] Step 2: run → fails.
- [ ] Step 3: implement (join `imports` for the target's files; optionally add references-resolved-to-other-files). Deterministic + de-duplicated.
- [ ] Step 4: CLI command + output.
- [ ] Step 5: full suite + clippy → green.
- [ ] Step 6: commit `feat(graph): dependency graph and sb dependencies`.

---

## Task 7: `find_impact`

**Files:** Modify `src/graph/deps.rs` (or add `src/graph/impact.rs`), `src/db/queries.rs`, `src/cli/*`, `src/main.rs`; tests.

**Interfaces produced:**
- `graph::impact::find_impact(conn, name) -> Result<Vec<Symbol>>`: the transitive set of symbols/functions that (directly or indirectly) reference `name`. Computed by repeatedly expanding: references to `name` → their `container` symbols → references to those, until fixpoint. Deterministic (sorted worklist, visited set), terminates on cycles.
- CLI `sb impact <name>`.

- [ ] Step 1 (TDD): fixture `a()` called by `b()` called by `c()`; assert `find_impact("a")` returns `{b, c}` (transitive), ordered and de-duplicated; a symbol with no callers returns empty; a cycle `x<->y` terminates.
- [ ] Step 2: run → fails.
- [ ] Step 3: implement BFS/worklist over `references.container`. Deterministic ordering; visited set prevents infinite loops.
- [ ] Step 4: CLI command + output.
- [ ] Step 5: full suite + clippy → green.
- [ ] Step 6: commit `feat(graph): transitive impact analysis and sb impact`.

---

## Task 8: JSON API (`sb serve`)

**Files:** Add `src/api/mod.rs`; modify `src/lib.rs`, `src/cli/*`, `src/main.rs`, `Cargo.toml`; integration test.

**Interfaces produced:**
- `api::serve(addr, db_path) -> Result<()>` using `tiny_http`: `GET /symbol/{name}` → JSON `{ "definition": [...], "references": [...], "implementations": [...], "dependencies": [...], "callers": [...] }`; `GET /health` → `{"status":"ok"}`. Uses `serde`/`serde_json`; domain types get `#[derive(Serialize)]` (via a serializable DTO layer to avoid leaking `PathBuf` oddities — serialize paths as strings).
- CLI `sb serve [--port 7645]`.

- [ ] Step 1 (TDD): integration test that indexes a fixture into a temp db, starts `api::serve` on an ephemeral port in a thread, issues `GET /symbol/AuthService` (using `std::net::TcpStream` or a tiny client), and asserts the JSON contains the definition with a string path; `GET /health` returns ok. Determinism: arrays ordered.
- [ ] Step 2: run → fails.
- [ ] Step 3: add deps (`tiny_http`, `serde` with derive, `serde_json`); implement DTOs + handlers + router. No async runtime.
- [ ] Step 4: CLI `sb serve`; keep diagnostics on stderr.
- [ ] Step 5: full suite + clippy → green; `cargo build --release`.
- [ ] Step 6: commit `feat(api): JSON API server (sb serve) exposing symbol intelligence`.

---

## Task 9: Docs Update

**Files:** Modify `second_brain/README.md`; ensure rustdoc coverage (`-W missing_docs`).

- [ ] Step 1: update README with the new commands (`watch`, `implementations`, `dependencies`, `impact`, `serve`), the JSON API shape, and the v0.2 resolution model (module/import-aware deterministic resolver). Note that Stack Graphs remains an alternative resolution backend.
- [ ] Step 2: `cargo clippy --all-targets -- -D warnings -W missing_docs` clean; `cargo test`; `cargo build --release`.
- [ ] Step 3: commit `docs: document v0.2 commands, JSON API, and resolution model`.

---

## Self-Review
- Spec coverage: incremental indexing (T3), cross-file references/resolution (T2+T4), dependency graph (T6), implementations (T5), impact (T7), JSON API (T8) — all present. Stack Graphs replaced by an in-house deterministic resolver (documented decision).
- Determinism: every query orders results; resolver uses documented deterministic tiers; impact uses a sorted worklist + visited set.
- Backward compat: migration runner upgrades v1 dbs (T1).
- Each task ends green with a commit.
