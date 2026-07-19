# SecondBrain Real-World Validation Plan

**Date:** 2026-07-19  
**Binary:** `second_brain` crate v1.0.0 (`sb`)  
**Goal:** Validate SecondBrain against (1) this repository via Cursor-oriented workflows and (2) a public open-source Rust project from GitHub.

---

## 1. Environments

| Target | Path / Source | Languages |
|--------|---------------|-----------|
| A — Current project | `/Users/ashokdudhade/os/second-brain` | Rust (+ docs) |
| B — OSS GitHub | Clone `https://github.com/BurntSushi/walkdir` (small, pure Rust, well-known) | Rust |

Workspaces for runs:
- Build: `DEVELOPER_DIR=/Library/Developer/CommandLineTools cargo build --release -p second_brain` from `second_brain/`
- Binary: `second_brain/target/release/sb` (or cargo target dir)

---

## 2. Cursor Integration Checklist

SecondBrain is meant to power agents. Validate the Cursor-facing surfaces:

### 2.1 MCP (`sb mcp`)
1. Build release `sb`.
2. Configure Cursor MCP (example):
   ```json
   {
     "mcpServers": {
       "secondbrain": {
         "command": "/ABS/PATH/second_brain/target/release/sb",
         "args": ["mcp"],
         "cwd": "/ABS/PATH/to/indexed/repo"
       }
     }
   }
   ```
3. From that `cwd`, run `sb index .` first so `.secondbrain/index.db` exists.
4. Automated stand-in (no GUI): send framed JSON-RPC `initialize` → `tools/list` → `tools/call` for `definition` / `references` / `impact` against the index; assert non-empty structured results for known symbols.

### 2.2 CLI (agent-parseable stdout)
Commands to exercise: `index`, `definition`, `references`, `callers`, `implementations`, `dependencies`, `impact`, `serve`.

### 2.3 JSON API (`sb serve`)
1. `sb index .` then `sb serve --port <ephemeral>`.
2. `GET /health` → `{"status":"ok"}`.
3. `GET /symbol/<KnownSymbol>` → JSON with ordered arrays.

---

## 3. Test Cases (both targets unless noted)

| ID | Case | Pass criteria |
|----|------|---------------|
| T1 | Fresh index | `sb index .` exits 0; prints Indexed/Skipped/Removed/Errors; creates `.secondbrain/index.db` |
| T2 | Incremental skip | Second `sb index .` without edits → `indexed=0`, `skipped>0` |
| T3 | Definition | `sb definition <known>` prints `path:line:col` with real file under repo |
| T4 | References / callers | Non-zero or explicit empty message on stderr; no panic |
| T5 | Implementations | For a trait/interface name present in corpus (Rust `trait` / skip if none) |
| T6 | Dependencies | Returns ordered module paths or empty |
| T7 | Impact | Terminates; deterministic repeat |
| T8 | Serve health + symbol | HTTP 200; valid JSON |
| T9 | MCP tools/list + definition | Framed RPC succeeds |
| T10 | Determinism | Two identical queries produce identical stdout |
| T11 | Multi-lang (A only) | Index finds symbols in `.rs` under `second_brain/src` |
| T12 | Path stability | Index with `.` twice; no mass `removed` churn |

---

## 4. Execution Script Outline

1. Build release `sb`.
2. Run suite A into `reports/run-A/`.
3. Clone walkdir shallow into `/tmp/sb-oss-walkdir`, run suite B into `reports/run-B/`.
4. Aggregate into HTML report.

---

## 5. Pass / Fail Rubric

- **PASS:** Command exit 0 (or expected non-zero only for missing symbol with clean message); outputs match schema; no panic.
- **FAIL:** Panic, non-zero exit on happy path, empty index when sources exist, non-deterministic output, hung HTTP.
- **WARN:** Empty results for optional queries; known limitations (name-based impact residual, etc.).
