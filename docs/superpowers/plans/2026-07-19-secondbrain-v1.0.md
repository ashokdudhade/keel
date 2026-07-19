# SecondBrain v1.0 Implementation Plan

> **For agentic workers:** Continuous execution / SDD. Checkbox steps for tracking.

**Goal:** Ship SecondBrain 1.0 — stable library + CLI APIs, multi-language monorepo support as a first-class feature, a documented community plugin registration surface, polished impact analysis, and complete docs.

**Architecture:** No rewrite. Harden what exists: public `lib` API surface with semver 1.0.0; monorepo indexing verified across Rust+TS+Go in one tree; `Registry::register` for external plugins; impact already exists — add README/API stability guarantees and a few polish items from the v0.2 defer list that are cheap (file sort already done; add `PRAGMA user_version` already at 2 — bump schema only if needed; OnceLock for Rust queries optional).

**Tech Stack:** Unchanged from v0.3.

## Global Constraints

Same as prior versions. Additionally:
- Crate version **1.0.0**. Breaking changes only with clear CHANGELOG notes (none expected if we only add APIs).
- Public library API must be intentional: re-export the stable entry points agents/tools need.
- Determinism preserved.

---

## Task 1: Stable Public Library API

**Files:** `src/lib.rs`, maybe thin `src/prelude.rs`; tests; bump toward 1.0 in this or final task.

**Interfaces (re-export from `lib.rs`):**
- `Index` facade OR documented free functions: `index_repository`, `open_db` helper, query helpers (`find_definition`, `find_references`, `find_callers`, `find_implementations`, `find_dependencies`, `find_impact`, `resolve_definition`).
- Prefer a small `SecondBrain` / `Index` struct:
  ```rust
  pub struct Index { conn: Connection }
  impl Index {
      pub fn open(path: impl AsRef<Path>) -> Result<Self>;
      pub fn open_in_memory() -> Result<Self>;
      pub fn index_path(&mut self, root: &Path) -> Result<IndexStats>;
      pub fn definition(&self, name: &str) -> Result<Vec<Symbol>>;
      // ... references, callers, implementations, dependencies, impact
  }
  ```
  Internals stay in modules; facade is the stable surface.

- [ ] TDD: library test using `Index::open_in_memory`, index a fixture, query definition.
- [ ] Implement facade without breaking existing CLI (CLI can call facade or keep using modules).
- [ ] Commit `feat(api): stable Index facade for 1.0 library consumers`.

---

## Task 2: Multi-Language Monorepo Support

**Files:** Integration tests; maybe `index/worker.rs` if extension filtering is Rust-only.

**Verify/fix:**
- `collect_*_files` must collect all extensions registered in the Registry (not only `.rs`). Rename/generalize `collect_rust_files` → `collect_source_files(registry)` filtering by any registered extension.
- Integration test: one temp repo with `a.rs`, `b.ts`, `c.go`; single `index_repository`; definitions found for symbols from all three languages.

- [ ] TDD failing if crawl is still `.rs`-only.
- [ ] Implement extension-aware crawl.
- [ ] Commit `feat(index): multi-language monorepo crawl via plugin extensions`.

---

## Task 3: Community Plugin Registration Surface

**Files:** `languages/mod.rs`; docs; test with a tiny fake plugin.

**Interfaces:**
- `Registry::empty() -> Self`
- `Registry::register(&mut self, plugin: Box<dyn LanguagePlugin>)`
- `Registry::with_defaults()` keeps built-ins.
- Optional: `index_repository_with_registry(root, conn, &Registry)` so callers can inject plugins — OR store registry on `Index`. Prefer `index_repository` gaining an overload/`with_registry` parameter without breaking: add `index_repository_with(root, conn, &Registry)` and have `index_repository` call `with_defaults()`.

- [ ] TDD: custom plugin with extension `toy` extracting one symbol; index a `.toy` file via custom registry.
- [ ] Commit `feat(lang): Registry::register for community language plugins`.

---

## Task 4: Impact + Deferred Polish

**Files:** `graph/impact.rs`, `languages/rust.rs` (OnceLock for queries), schema comment/docs.

- Confirm impact handles cycles (already) — add/strengthen test if thin.
- Cache compiled tree-sitter `Query` for Rust via `OnceLock` (v0.2 defer #4).
- Document known limitations (TS module_path, Go impls, name-based impact).

- [ ] OnceLock cache + test still green.
- [ ] Commit `perf(lang): cache Rust tree-sitter queries with OnceLock`.

---

## Task 5: Docs, CHANGELOG, 1.0.0 Release

**Files:** README, CHANGELOG.md, Cargo.toml version `1.0.0`, design spec roadmap note.

- Document stable `Index` API, multi-lang monorepos, plugin registration, MCP, HTTP API, full CLI.
- CHANGELOG summarizing v0.1 → v1.0.
- clippy missing_docs + test + release build.

- [ ] Commit `release: SecondBrain 1.0.0`.

---

## Self-Review
Proposal v1.0: multi-language monorepos ✅, impact ✅, plugin system ✅, stable APIs ✅, documentation ✅.
