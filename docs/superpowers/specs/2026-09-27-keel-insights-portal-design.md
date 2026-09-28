# Keel Insights Portal + Miss/Hygiene Fixes Design

**Date:** 2026-09-27
**Status:** Draft (planning)
**Target version:** 1.4.0
**Companion plan:** `docs/superpowers/plans/2026-09-27-keel-muse-experience.md`

## 1. Goal

Give every Keel user a local, zero-setup page that answers "what is Keel doing
for me?": index health, real query usage, answer quality (confidence mix, miss
recovery), and recent activity — styled like an insights dashboard, served from
the project's own data. Alongside it, fix the small correctness bugs that make
Keel look untrustworthy when an agent drives it (wrong-path indexing, wrong
empty notes, unmarked external deps).

Success bar: a skeptic runs `keel serve`, opens `/insights`, and sees true
statements about their index and their (or their agent's) usage within one
minute, with no account, no network, and no new install step.

Out of scope for this design:

- Authentication, multi-user, or remote hosting (localhost only, like `serve`).
- New languages; full data-flow recall (bare-identifier capture only, §6).
- Semantic/fuzzy discovery beyond the existing did-you-mean notes.
- Global cross-project analytics (per-project only).

## 2. Constraints

- Local-first, deterministic, offline-capable: the page must render with no
  external requests (no CDNs, fonts, or telemetry endpoints).
- Zero new crates. Reuse `tiny_http` (server), `serde_json` (payloads),
  `rusqlite` (live stats). Charts are hand-rolled inline SVG + vanilla JS.
- Additive surfaces only: new routes, one new module, one new data file. No
  changes to existing CLI/MCP/HTTP response shapes except the specced notes.
- Privacy: usage data never leaves the project directory; collection is
  on-by-default but one env var off (`KEEL_NO_USAGE_LOG=1`).

## 3. Usage telemetry

### 3.1 Event sink

New module `keel/src/usage.rs`. Every query and index pass appends one JSON
object per line to `.keel/usage.jsonl`, colocated with the index it describes
(resolved the same way MCP resolves the db: the file lives next to the
`index.db` that served the answer).

Event schema (v1, all fields stable):

```json
{
  "ts": 1790550000,
  "surface": "mcp",
  "tool": "callers",
  "target": "create_order",
  "module": null,
  "hits": 2,
  "confidence": "high",
  "tier": "2",
  "suggested": false,
  "empty_index": false,
  "ms": 3
}
```

- `surface`: `cli` | `mcp` | `http`.
- `tool`: `definition` | `references` | `callers` | `implementations` |
  `dependencies` | `impact` | `index`.
- `tier`: `resolution_tier` rendered as string (`0`, `1`, `2`, `3`, `mixed`).
- `suggested`: a did-you-mean note was emitted. `empty_index`: the
  empty-index note was emitted.
- Index passes emit no usage events (dropped in implementation): freshness
  comes from the `last_indexed` meta stamp (§4) instead.

### 3.2 Emission points

- CLI + MCP: the six `facade::*_with_meta` functions (single choke point).
  Callers pass their surface; timing wraps the facade call.
- HTTP: `api::symbol_intelligence` emits once per `/symbol/{name}` request
  (it reads `queries::` directly and bypasses the facade). The aggregate has
  no trust envelope, so HTTP events log `confidence`/`tier` as `unknown`
  (grouped separately in the dashboard) with summed hit counts.
- The CLI (no connection in hand) logs via `usage::log_query_at` against
  `./.keel/usage.jsonl`; MCP/HTTP resolve the path from their connection
  (`PRAGMA database_list`, in-memory skips).
- MCP silence rule preserved: logging touches only the JSONL file, never
  stdout; rotation is silent.

### 3.3 Rotation and opt-out

- Cap `.keel/usage.jsonl` at 5 MiB: on crossing, rename to `usage.jsonl.1`
  (dropping any older `.1`) and start fresh. Best-effort; failures never fail
  queries.
- `KEEL_NO_USAGE_LOG=1` disables all writes. The portal shows a "collection
  off" state instead of zeros.

## 4. Serving

`keel serve` (127.0.0.1 only, unchanged default) gains:

- `GET /insights` → `text/html`, the embedded dashboard
  (`keel/src/insights.html` via `include_str!`, ~single file).
- `GET /api/insights` → `application/json`:

```json
{
  "project": {"root": "/path/to/proj", "index_db": ".keel/index.db"},
  "index": {"files": 44, "symbols": 812, "references": 1930,
            "format": 1, "format_current": true, "writer": "1.4.0",
            "last_rebuild": 1790550000},
  "usage": {"events": 128, "since_ts": 1790400000,
            "by_tool": {"definition": 40, "callers": 30},
            "by_confidence": {"high": 100, "medium": 20, "low": 8},
            "misses": 12, "recovered": 5, "empty_index_hits": 1},
  "top_symbols": [{"name": "serve", "n": 9}],
  "hourly": [{"ts": 1790550000, "cli": 3, "mcp": 1, "http": 0}],
  "health": [{"name": "daemon", "ok": true, "detail": "running"}],
  "recent": [ ... last 20 raw events ... ],
  "collection": "on"
}
```

`index` stats come from live `COUNT(*)` queries; `usage` rollups from parsing
`usage.jsonl` (+`.1`) per request (small files; no cache in v1). `serve`
prints the insights URL on startup next to the API line.

## 5. Dashboard

Single page, dark, card layout (insight-page style). Sections:

1. **Health strip:** index files/symbols/references, format status
   (current/stale + rebuild hint), writer version, daemon + watch state for
   this project (reuse `doctor_checks` via a JSON projection).
2. **Usage over time:** SVG bar/line chart of events per hour (last 7 days),
   stacked by surface (cli/mcp/http).
3. **Answer quality:** donut of confidence mix; miss rate; "recovered by
   suggestion" count — the value story for skeptics.
4. **Top symbols + recent queries table:** what the team/agents actually ask;
   each row links nothing (no code viewer in v1) but shows tool, target,
   hits, confidence, latency.
5. **Empty states:** no usage yet ("run a query or connect your agent —
   here is the MCP snippet"), collection off, index missing/stale (reuse
   doctor hint strings).

Auto-refresh every 30s via `fetch('/api/insights')`; full render in vanilla
JS (< 600 lines). No frameworks, no external assets; works from `file://`
only for layout (data needs `serve`).

## 6. Bare-identifier recall (F3, scoped)

Python, TypeScript, and JavaScript plugins gain identifier capture in two
positions that currently yield nothing: **type annotations** and
**call-argument values** (decorators-as-calls and direct calls already work).
Implemented as walker arms: `type`/`type_annotation` subtrees emit
`ReferenceKind::Type` (nested levels handle themselves; `call` subtrees fall
through; PEP 695 alias targets excluded), and `argument_list`/`arguments`
emit direct identifiers plus keyword/object values as the new
`ReferenceKind::Value` (keyword names and receivers stay quiet). Python class
bases and decorator args share the `argument_list` shape and are covered too.
Rust/Go bare-value positions remain a follow-up.

## 7. Small correctness fixes

- **F1 — index at the target root:** `keel index <path>` resolves `<path>`
  and creates/uses `<path>/.keel/index.db` (not the cwd's); the owning root
  is derived from the db location at runtime (never recorded, so copies and
  moves can't strand it). Query auto-index crawls the owning root, never the
  cwd. Regression test covers out-of-tree index + query (`removed 0`).
- **F2 — honest empty impact:** when definitions exist but nothing is
  impacted, the note reads "No impacted symbols found." (plus the usual
  candidate-radius note when multi-def). Nonsense names keep the canonical
  "No matching symbols found." across CLI/MCP/JSON.
- **F5 — external markers:** `Dependency` gains `external: bool` (true when
  no defining file is indexed); CLI prints an `external` tag, MCP/JSON carry
  the field. Additive only.
- **F4 — confused edges:** investigation-first (see plan Phase 2): committed
  repro, root-cause at the edge-acceptance rule, fix or split to follow-up.

## 8. Non-goals (v1)

Code viewer, diff views, per-agent attribution, alerting, export buttons,
global dashboards. The portal reports; it does not edit, re-index, or manage
the daemon (buttons would need POST routes + CSRF thought — deferred).

## 9. Validation

- `cargo test`, strict clippy, and `scripts/mcp-trust-smoke.py` green.
- New regression tests: F1 out-of-tree flow, F2 note matrix (hit / impacted /
  empty-impact / nonsense × CLI JSON + MCP), F3 fixtures, usage rotation +
  opt-out, `/api/insights` schema shape.
- Release-binary E2E: `serve` → `curl /insights` (200, `text/html`, zero
  external URLs) → `curl /api/insights` (validates against §4 schema).
- Manual: open `/insights` in a browser over a real indexed project with a
  week of simulated usage; confirm every number matches `sqlite3` counts and
  JSONL contents.
