# Keel v0.3 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (or continuous YOLO execution). Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose Keel to AI agents via MCP, and add TypeScript + Go language plugins so multi-language repos index through the same core.

**Architecture:** Keep the language-agnostic core. Add `keel mcp` (stdio JSON-RPC MCP server wrapping existing queries). Add `languages/typescript.rs` and `languages/go.rs` implementing `LanguagePlugin`, register them in `Registry::with_defaults`. Indexer already dispatches by extension.

**Tech Stack:** As v0.2, plus `tree-sitter-typescript`, `tree-sitter-go`. MCP is a hand-rolled stdio JSON-RPC 2.0 server (no heavy SDK) implementing `initialize`, `tools/list`, `tools/call`, `ping`.

## Global Constraints

Same as v0.2 (edition 2021, no unwrap/expect outside tests, no unsafe, PathBuf, thiserror/anyhow split, Rustdoc, quoted `"references"`, determinism, DEVELOPER_DIR workaround).
- MCP speaks over **stdio** only (line-delimited or Content-Length framed JSON-RPC per MCP streamable HTTP/stdio convention — use **Content-Length framed** messages as used by Cursor/Claude MCP stdio).
- Tools must be deterministic wrappers over existing library APIs; no LLMs.
- If `tree-sitter-typescript` Cargo resolution conflicts with `tree-sitter 0.25`, prefer a git/path override or `tree-sitter-language`-only usage that still compiles with 0.25 — do not downgrade the Rust plugin.

---

## Task 1: MCP Server (`keel mcp`)

**Files:** Create `src/mcp/mod.rs`; modify `cli/`, `main.rs`, `lib.rs`, `Cargo.toml` if needed; integration test.

**Interfaces:**
- `mcp::serve(db_path: &Path) -> Result<()>` — read framed JSON-RPC from stdin, write framed responses to stdout; logs to stderr only.
- Tools (names stable):
  - `definition` { name }
  - `references` { name }
  - `callers` { name }
  - `implementations` { name }
  - `dependencies` { name }
  - `impact` { name }
  - `index` { path } — runs `index_repository`, returns IndexStats JSON
- Each tools/call returns JSON text content with the same DTO shapes as the HTTP API where applicable.
- CLI: `keel mcp` (opens `.keel/index.db` in CWD; create/init schema if missing for read tools; index tool writes).

- [ ] Step 1 (TDD): unit tests for framing encode/decode; a test that drives `handle_message` for `initialize` and `tools/list` without a live stdin loop.
- [ ] Step 2: implement framing + dispatcher + tool handlers reusing `db::queries`, `graph::{resolve,deps,impact}`, `index::index_repository`, and API DTOs (or shared serialization).
- [ ] Step 3: wire CLI; integration test optional (spawn `keel mcp`, send initialize, assert tools/list). Prefer non-flaky unit tests of the handler.
- [ ] Step 4: full suite + clippy green. Commit `feat(mcp): stdio MCP server with code-intelligence tools`.

---

## Task 2: TypeScript Language Plugin

**Files:** Create `src/languages/typescript.rs`; modify `languages/mod.rs`; tests.

**Interfaces:**
- `TypeScriptPlugin` implementing `LanguagePlugin` for extensions `ts`, `tsx`, `mts`, `cts` (and optionally `js`/`jsx` if the same grammar covers them — prefer TS/TSX only for v0.3).
- Extract: functions, classes, interfaces, type aliases, methods, enums as symbols with module_path approximated from file path relative to package root OR `crate`-like `module` from path stem (document choice: use path-based module_path like `src/auth/service` without extension).
- References: call expressions, property/method calls.
- Imports: `import … from '…'` and `import { a as b } from '…'` (module_path = source string, alias when present).
- Impls: default empty OR map `implements` clauses to ImplRecord when straightforward.

- [ ] Step 1 (TDD): fixture TS source asserting class/interface/function symbols, an import, a method call reference.
- [ ] Step 2: implement with tree-sitter queries; register in `Registry::with_defaults`.
- [ ] Step 3: integration test indexing a tiny `.ts` fixture via `index_repository` finds the symbol.
- [ ] Step 4: suite + clippy green. Commit `feat(lang): TypeScript/TSX tree-sitter plugin`.

---

## Task 3: Go Language Plugin

**Files:** Create `src/languages/go.rs`; modify `languages/mod.rs`; tests.

**Interfaces:**
- `GoPlugin` for extension `go`.
- Symbols: functions, methods, types (struct/interface), consts.
- module_path: package name from `package` clause (e.g. `auth`).
- Imports: `import "pkg"` and `import alias "pkg"`.
- References: call expressions, selector calls.
- Impls: Go has no `impl Trait for Type` — leave `extract_impls` default empty (interfaces satisfied implicitly; document as future work). Optionally record `type X interface` as Trait-kind symbols.

- [ ] Step 1 (TDD): fixture Go source asserting package-aware symbols + import + call.
- [ ] Step 2: implement + register.
- [ ] Step 3: integration test indexing `.go` file.
- [ ] Step 4: suite + clippy green. Commit `feat(lang): Go tree-sitter plugin`.

---

## Task 4: Docs + Version Bump Prep

**Files:** `README.md`, rustdoc; bump crate version to `0.3.0`.

- [ ] Document `keel mcp`, TS/Go support, MCP tool list.
- [ ] `cargo clippy --all-targets -- -D warnings -W missing_docs`; `cargo test`; `cargo build --release`.
- [ ] Commit `docs: document v0.3 MCP server and TypeScript/Go plugins`.

---

## Self-Review
- Proposal v0.3 items: MCP ✅, TypeScript ✅, Go ✅.
- Multi-language indexing piggybacks on existing Registry/extension dispatch (v1.0 monorepo support is then mostly docs + polish).
