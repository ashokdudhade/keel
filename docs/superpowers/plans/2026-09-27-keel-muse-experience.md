# Keel + Muse Experience Plan

> **For agentic workers:** implement task-by-task in phase order. Steps use checkbox (`- [ ]`) syntax for tracking. Phase 0 first: the tree currently does not compile (E0308, `keel/src/daemon.rs:706`).

**Goal:** Make Keel demonstrably valuable in a Muse-driven workflow: fix the dogfood-blocking bugs, close the worst recall gap, and ship a local Insights portal (`keel serve` → `/insights`) showing index health and real usage, so users can see what Keel does for them.

**Architecture:** Repair-then-extend. Stabilize the tree (build + suite green), land correctness fixes behind the existing `QueryResult` envelope, add append-only per-project usage logging on the facade query path, and serve a zero-dependency embedded dashboard through the existing `tiny_http` server. No new crates, no build step, no network calls.

**Tech Stack:** Rust (`keel` crate), rusqlite, tiny_http, one self-contained HTML/SVG/JS page embedded via `include_str!`.

**Target version:** 1.4.0

## Dogfood findings (all reproduced on the v1.3.1 release binary)

- **F1 (Blocker):** `keel index <other-path>` writes the index to the *cwd*, and the next query's auto-index wipes it (`removed 4867`, 52s). Out-of-tree indexing is destructive. Root: `run_index` uses cwd-relative `open_db` while `index_repository` stores paths relative to `path`.
- **F2 (Major):** Empty `impact` says "No matching symbols found." even when the symbol exists (CLI `--json` and MCP). Wrong note for "nothing impacted".
- **F3 (Major):** Bare-name usages are invisible: callback values (`to_thread(assert_public_http_url, …)`) and type annotations (`body: ScrapeBody`) produce no references/callers, understating impact. Direct calls work.
- **F4 (Major):** `dependencies main` lists `tests.test_main`, which `main.py` never imports. Suspect: `acceptable_top_match` auto-accepts single tier-3 matches (`resolve.rs`), creating confused edges; likely pollutes `impact` too. Needs root-cause confirmation with the committed repro.
- **F5 (Minor):** `dependencies` mixes stdlib/external modules (`file: null`) with project modules with no marker — agent noise.
- **F6 (Positive):** 4867-file index in ~26s with 0 errors; queries instant; 34MB index. Scale is not the problem.

## Success Criteria

- `cargo build`, full `cargo test`, `cargo clippy --all-targets -- -D warnings`, and `scripts/mcp-trust-smoke.py` pass on a clean tree.
- Repro scripts for F1/F2 exist as regression tests; F1 flow indexes at the target and never wipes on query.
- Empty `impact` on an existing symbol reports a "nothing impacted" note, never "No matching symbols found."
- F4 root-caused; confused edge gone or guarded with a documented rule.
- `keel serve` exposes `/insights` (dashboard) and `/api/insights` (JSON) on 127.0.0.1; dashboard renders usage, confidence mix, miss recovery, and index health from real local data with no external requests.
- READMEs + CHANGELOG describe the new behavior; accuracy claims carry method/N/dates.

## Key Decisions

- **D1 — Portal is an embedded single file, not an app.** One HTML/SVG/JS page via `include_str!`, served by the existing `tiny_http` server. Rejected: React/Vite build (deploy weight for a local page), separate binary (install friction), `file://` static page (no live data).
- **D2 — Telemetry is per-project JSONL, not global.** `.keel/usage.jsonl` next to `index.db`: one JSON object per query/index event, 5MB rotation, `KEEL_NO_USAGE_LOG=1` opt-out, local-only. Rejected: global log (mixes projects, worse privacy posture), SQLite table (harder to inspect by hand, couples writers to the index lock).
- **D3 — Log at the facade, serve from `keel serve`.** All CLI/MCP queries funnel through `facade::*_with_meta`; the HTTP API's direct `queries::` reads get one explicit call. Portal reads JSONL + live `COUNT(*)` stats; no new storage engine.
- **D4 — F3 scoped to annotations/arguments first.** Capture identifiers in annotation and call-argument positions, Python + TypeScript first (observed surfaces); other plugins follow the same helper later. Full data-flow is out.
- **D5 — F4 is investigation-first, timeboxed.** Repro test first; if the fix escapes the dependency-edge rule, it splits into its own plan rather than blocking the portal.

## File map

| Path | Responsibility |
|------|----------------|
| `keel/src/daemon.rs` | Fix E0308; existing doctor/stop work |
| `keel/src/cli/commands.rs` | F1: index at target root |
| `keel/src/facade.rs` | F2 note; usage-event emission point |
| `keel/src/graph/resolve.rs` | F4 investigation |
| `keel/src/graph/deps.rs` | F4/F5: edge rule + external marker |
| `keel/src/languages/*.rs` | F3: annotation/argument capture |
| `keel/src/usage.rs` (new) | JSONL event log, rotation, opt-out |
| `keel/src/api/mod.rs` | `/insights` + `/api/insights` routes |
| `keel/src/insights.html` (new) | Embedded dashboard page |
| `keel/tests/*.rs` | Regression tests per task |
| READMEs, CHANGELOG, AGENTS.md | Behavior + accuracy-claim updates |

## Steps

### Phase 0 — Tree green (prerequisite)

- [x] Fix E0308 at `daemon.rs:706` (`open_doctor_db` maps `rusqlite::Error` into `KeelError::Io`'s `std::io::Error` source; use `?`/Database instead). Then build, full test suite, strict clippy, smoke test.

### Phase 1 — Correctness (F1, F2)

- [x] F1: `keel index <path>` creates/uses `<path>/.keel/index.db` (not the cwd's); query auto-index crawls the owning root. Regression test with out-of-tree path (`removed 0`, correct relative paths).
- [x] F2: empty `impact` on existing symbol(s) emits a "nothing impacted" note through the facade (CLI/MCP/JSON); nonsense names keep the canonical miss. Extend facade + smoke assertions.

### Phase 2 — Trust edges (F4, F5)

- [x] F4: commit repro fixture, root-cause the confused edge, fix at the edge-acceptance rule; `deps` regression tests. Timebox: split to follow-up plan if it escapes the rule.
- [x] F5: mark unresolved/external dependencies (`external: true` in DTOs, `external` tag in CLI) so agents can filter stdlib noise.

### Phase 3 — Recall (F3)

- [x] Capture bare identifiers in annotation + call-argument positions in the Python plugin; extend the shared helper to TypeScript; per-language regression tests with callback/annotation fixtures.

### Phase 4 — Insights portal

- [x] `usage` module: JSONL event schema, rotation, opt-out; unit tests.
- [x] Emit events on all query paths (facade choke point + HTTP direct reads + index passes); verify no stdout/stderr pollution for MCP.
- [x] `/api/insights` JSON and `/insights` embedded dashboard (cards, SVG charts, recent-queries table); release-binary E2E with curl; manual browser pass.

### Phase 5 — Docs

- [x] CHANGELOG (1.4.0), README behavior updates (`index <path>`, impact notes, insights), accuracy claims with method/N/dates, AGENTS.md refresh.

## Validation Plan

- Per phase: `cargo test` and `cargo clippy --all-targets -- -D warnings` from `keel/`.
- Trust gate after Phases 1–2: `keel index . && KEEL_BIN=… python3 scripts/mcp-trust-smoke.py` from root.
- F1: out-of-tree `index` + query shows `removed 0` and correct relative paths.
- F2/F4/F5: new regression tests plus `--json` spot checks.
- Portal: `keel serve` → `curl /api/insights` schema check; `/insights` returns 200 `text/html` with zero external URLs; manual browser walkthrough by the user.
- Highest-risk validation: F4 root-cause (may implicate the shared resolve rule) and portal data freshness across CLI/MCP/HTTP writers.

## Risks / Open Questions

- Prior uncommitted work (review items #1–#5, #7, #3, #2 + partial #4) is the base; Phase 0 must land before anything else. No open technical questions; F4's exact edge mechanism is the one investigation item, with a committed repro to settle it.
