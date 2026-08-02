# Keel Query Trust Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make all six graph queries trustworthy on common paths across Rust, TS/JS, Python, and Go, with additive confidence metadata.

**Architecture:** Fix module identity in language plugins, add shared target normalization, route every query through ranked resolve, and return a `QueryResult` envelope (`confidence` / `resolution_tier` / `notes`) without breaking existing `Vec`-returning APIs.

**Tech Stack:** Rust, Tree-sitter plugins, SQLite (`rusqlite`), existing `keel` CLI/MCP/HTTP surfaces, Cargo tests.

## Global Constraints

- Target version: 1.2.0
- Additive API only: keep `Index::definition` etc. returning `Vec<_>`; add `*_with_meta` / envelope
- Soft uncertainty → success + `confidence: low` + notes (never fail the tool call)
- No Stack Graphs, no LSP backends, no discovery UX in this plan
- Re-index required after module-identity changes (document; no schema migration)

---

## File map

| Path | Responsibility |
|------|----------------|
| `keel/src/graph/query_result.rs` (new) | `Confidence`, `QueryResult<T>`, envelope helpers |
| `keel/src/graph/target.rs` (new) | `normalize_target` → files + preferred module |
| `keel/src/graph/mod.rs` | Export new modules |
| `keel/src/languages/rust.rs` | File-module `module_path` from path layout |
| `keel/src/languages/{typescript,javascript,python,go}.rs` | Import/module id alignment for common paths |
| `keel/src/graph/{deps,impact,resolve}.rs` | Use normalize + envelope-aware helpers |
| `keel/src/facade.rs` | `*_with_meta` methods |
| `keel/src/cli/commands.rs` | `--json` envelope output |
| `keel/src/mcp/mod.rs`, `keel/src/api/mod.rs` | Envelope fields in responses |
| `keel/tests/common_path.rs` (new) | Per-language fixture integration tests |
| `keel/README.md`, root `README.md` | Resolution model + re-index + confidence |

---

### Task 1: Rust file-module identity

**Files:**
- Modify: `keel/src/languages/rust.rs`
- Test: unit tests in `keel/src/languages/rust.rs` (`#[cfg(test)]`)

**Interfaces:**
- Consumes: `LanguagePlugin::extract_symbols(path, source)`
- Produces: symbols in `src/mcp/mod.rs` with `module_path == "crate::mcp"`; `src/lib.rs` stays `"crate"`; inline `mod foo { }` still nests as today

- [ ] **Step 1: Write the failing test**

Add to `rust.rs` tests:

```rust
#[test]
fn file_module_path_from_src_layout() {
    let plugin = RustPlugin;
    let src = "pub fn serve() {}\n";
    let syms = plugin
        .extract_symbols(Path::new("src/mcp/mod.rs"), src)
        .unwrap();
    let serve = syms.iter().find(|s| s.name == "serve").unwrap();
    assert_eq!(serve.module_path, "crate::mcp");
}

#[test]
fn nested_file_module_path() {
    let plugin = RustPlugin;
    let src = "pub struct Wire;\n";
    let syms = plugin
        .extract_symbols(Path::new("src/mcp/wire.rs"), src)
        .unwrap();
    assert_eq!(syms[0].module_path, "crate::mcp::wire");
}

#[test]
fn lib_rs_stays_crate_root() {
    let plugin = RustPlugin;
    let src = "pub struct Index;\n";
    let syms = plugin
        .extract_symbols(Path::new("src/lib.rs"), src)
        .unwrap();
    assert_eq!(syms[0].module_path, "crate");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd keel && cargo test file_module_path_from_src_layout nested_file_module_path lib_rs_stays_crate_root -- --nocapture`  
Expected: FAIL (`module_path` is `"crate"` for file modules)

- [ ] **Step 3: Write minimal implementation**

In `RustPlugin::extract_symbols` / `extract_references` / `extract_imports` containers, pass `path` and seed the module stack from file layout:

```rust
fn rust_file_module_path(path: &Path) -> Vec<String> {
    // Strip leading src/ (or lib/bin crate roots), drop lib.rs/main.rs/mod.rs
    // file stem, return segments: src/mcp/mod.rs → ["mcp"], src/mcp/wire.rs → ["mcp","wire"]
}
```

Seed `mods` with that vec before `walk_symbols`. Update `qualify_scope` for references similarly so containers use `crate::mcp::…`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cd keel && cargo test file_module_path_from_src_layout nested_file_module_path lib_rs_stays_crate_root`  
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add keel/src/languages/rust.rs
git commit -m "$(cat <<'EOF'
fix(rust): derive module_path from file layout for file modules

EOF
)"
```

---

### Task 2: Target normalization

**Files:**
- Create: `keel/src/graph/target.rs`
- Modify: `keel/src/graph/mod.rs`
- Modify: `keel/src/graph/deps.rs` (`files_for_target` → use normalize)
- Test: `keel/src/graph/target.rs` unit tests

**Interfaces:**
- Consumes: `queries::files_for_module_path`, `queries::find_definition`, file path lookups
- Produces:

```rust
pub struct ResolvedTarget {
    pub files: Vec<String>,
    pub preferred_module: Option<String>,
}

pub fn normalize_target(conn: &Connection, target: &str) -> Result<ResolvedTarget>;
```

- [ ] **Step 1: Write the failing test**

Insert symbols with `module_path = "crate::mcp"` in file `src/mcp/mod.rs`, then:

```rust
#[test]
fn normalize_accepts_module_path_and_file() {
    let conn = setup_mcp_fixture();
    let by_mod = normalize_target(&conn, "crate::mcp").unwrap();
    assert!(by_mod.files.iter().any(|f| f.ends_with("mcp/mod.rs")));
    let by_file = normalize_target(&conn, "src/mcp/mod.rs").unwrap();
    assert_eq!(by_mod.files, by_file.files);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cd keel && cargo test normalize_accepts_module_path_and_file`  
Expected: FAIL (module not found)

- [ ] **Step 3: Implement `normalize_target`**

Resolution order:

1. If `target` matches a file path in `files` table → that file; `preferred_module` = dominant `module_path` in file
2. Else if `files_for_module_path(target)` non-empty → those files; preferred = `target`
3. Else treat as symbol name → definition files; preferred = unique module if any

Wire `deps::files_for_target` to call `normalize_target`.

- [ ] **Step 4: Run tests**

Run: `cd keel && cargo test normalize_accepts_module_path_and_file graph::deps`  
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add keel/src/graph/target.rs keel/src/graph/mod.rs keel/src/graph/deps.rs
git commit -m "$(cat <<'EOF'
feat(graph): normalize query targets as symbol, module, or file

EOF
)"
```

---

### Task 3: QueryResult envelope types

**Files:**
- Create: `keel/src/graph/query_result.rs`
- Modify: `keel/src/graph/mod.rs`, `keel/src/lib.rs` (re-export if public)
- Test: unit tests in `query_result.rs`

**Interfaces:**
- Produces:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence { High, Medium, Low }

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueryResult<T> {
    pub results: Vec<T>,
    pub confidence: Confidence,
    pub resolution_tier: ResolutionTier, // 1 | 2 | 3 | Mixed
    pub notes: Vec<String>,
}

pub fn confidence_from_tiers(tiers: &[u8], multi_def: bool) -> Confidence;
```

- [ ] **Step 1: Write failing tests for confidence heuristics**

```rust
#[test]
fn high_when_all_tier_one_or_two() {
    assert_eq!(confidence_from_tiers(&[1, 2], false), Confidence::High);
}
#[test]
fn low_when_tier_three_dominates() {
    assert_eq!(confidence_from_tiers(&[3, 3], false), Confidence::Low);
}
```

- [ ] **Step 2: Run — expect FAIL (types missing)**

- [ ] **Step 3: Implement types + helpers**

- [ ] **Step 4: Run — expect PASS**

- [ ] **Step 5: Commit**

```bash
git add keel/src/graph/query_result.rs keel/src/graph/mod.rs keel/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(graph): add QueryResult envelope and confidence helpers

EOF
)"
```

---

### Task 4: Facade `*_with_meta` + deps/callers/impact correctness

**Files:**
- Modify: `keel/src/facade.rs`
- Modify: `keel/src/graph/deps.rs`, `impact.rs`, `resolve.rs` as needed
- Test: facade + graph unit/integration tests

**Interfaces:**
- Produces: `Index::definition_with_meta`, `references_with_meta`, `callers_with_meta`, `implementations_with_meta`, `dependencies_with_meta`, `impact_with_meta` each returning `QueryResult<_>`
- Existing `Vec` methods delegate to `.results` for compatibility

- [ ] **Step 1: Failing integration test** — temp crate with `src/lib.rs` (`mod mcp;`) + `src/mcp/mod.rs` (`use crate::api::X; fn serve(){}`) asserting `dependencies("crate::mcp")` non-empty after index

- [ ] **Step 2: Run — expect FAIL or empty**

- [ ] **Step 3: Implement with_meta methods; ensure deps uses normalize + imports**

- [ ] **Step 4: `cargo test` green for new tests + existing facade

- [ ] **Step 5: Commit**

```bash
git commit -m "$(cat <<'EOF'
feat(index): add with_meta queries and fix module dependency lookup

EOF
)"
```

---

### Task 5: TS/JS + Python + Go common-path alignment

**Files:**
- Modify: `keel/src/languages/typescript.rs`, `javascript.rs`, `python.rs`, `go.rs`
- Modify: `keel/src/graph/resolve.rs` if import matching needs path normalization helpers
- Test: per-plugin unit tests + shared normalize cases

**Interfaces:**
- Produces: relative imports resolve to the same module id as the defining file’s `module_path` on common layouts (`./foo`, `../bar`, Python relative, Go same-module package imports)

- [ ] **Step 1: Write one failing import-resolve test per language**

- [ ] **Step 2: Run — expect FAIL**

- [ ] **Step 3: Minimal normalize helpers in each plugin / shared resolve**

- [ ] **Step 4: Tests PASS**

- [ ] **Step 5: Commit**

```bash
git commit -m "$(cat <<'EOF'
fix(languages): align import module ids with defining file paths

EOF
)"
```

---

### Task 6: Common-path fixture integration suite

**Files:**
- Create: `keel/tests/common_path.rs`
- Create fixtures under `keel/tests/fixtures/common_path/{rust,ts,js,python,go}/`

**Interfaces:**
- Consumes: `Index::index_path` + `*_with_meta`
- Asserts non-empty deps, precise callers, impact notes when multi-def

- [ ] **Step 1: Add Rust fixture + failing test harness**

- [ ] **Step 2: Run — FAIL until fixtures match Task 1–4 behavior**

- [ ] **Step 3: Add remaining language fixtures/tests**

- [ ] **Step 4: `cargo test --test common_path` PASS**

- [ ] **Step 5: Commit**

```bash
git commit -m "$(cat <<'EOF'
test: add common-path fixtures for all five languages

EOF
)"
```

---

### Task 7: CLI `--json` + MCP/HTTP envelope fields

**Files:**
- Modify: `keel/src/cli/commands.rs`, `keel/src/cli/mod.rs` (clap `--json`)
- Modify: `keel/src/mcp/mod.rs`, `keel/src/api/mod.rs`
- Test: existing MCP/API tests updated for new JSON fields

**Interfaces:**
- CLI default output unchanged
- `keel definition Foo --json` prints envelope
- MCP tool results include `confidence`, `resolution_tier`, `notes` alongside existing arrays

- [ ] **Step 1: Failing test / assert JSON shape in API or MCP test**

- [ ] **Step 2: Implement**

- [ ] **Step 3: `cargo test` PASS**

- [ ] **Step 4: Commit**

```bash
git commit -m "$(cat <<'EOF'
feat: expose query confidence metadata in CLI JSON, MCP, and HTTP

EOF
)"
```

---

### Task 8: Docs + version note

**Files:**
- Modify: `keel/README.md` (Resolution model, confidence, re-index)
- Modify: root `README.md` briefly
- Modify: `keel/CHANGELOG.md` under Unreleased / 1.2.0
- Modify: `keel/Cargo.toml` version → `1.2.0` when ready to cut (or leave Unreleased until release)

- [ ] **Step 1: Document confidence semantics + re-index after upgrade**

- [ ] **Step 2: Manual smoke:** `rm -rf .keel && keel index . && keel dependencies crate::mcp` non-empty

- [ ] **Step 3: Commit**

```bash
git commit -m "$(cat <<'EOF'
docs: document query confidence and module-identity re-index

EOF
)"
```

---

## Spec coverage checklist

| Spec requirement | Task |
|------------------|------|
| Rust file-module identity | 1 |
| Target normalization | 2 |
| QueryResult envelope | 3, 7 |
| All six queries / with_meta | 4 |
| Dependencies fix | 2, 4 |
| TS/JS/Python/Go common paths | 5, 6 |
| Fixture pack | 6 |
| Soft uncertainty + notes | 3, 4, 7 |
| Docs / re-index | 8 |
| No Stack Graphs / discovery | honored (non-goals) |
