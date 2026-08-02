# Keel Query Trust — Common-Path Correctness Design

**Date:** 2026-08-01  
**Status:** Approved (brainstorming)  
**Target version:** 1.2.0  

## 1. Goal

Make Keel’s full graph query surface trustworthy on **common paths** across
all five shipped languages, so agents and engineers can rely on structural
answers in everyday code.

Success bar (chosen): import-aware resolution and module dependencies work
reliably for normal code; over-approximation only on hard cases, **documented
in the response** via additive metadata.

Out of scope for this design:

- Stack Graphs / LSP hybrid backends
- Daemon / MCP install friction (“zero-friction agent use”)
- Fuzzy / semantic discovery when the symbol name is unknown
- Refusing to answer on ambiguity (label instead of empty-on-unsure)

## 2. Constraints

- Local-first, deterministic, no LLMs or embeddings.
- Languages: Rust, TypeScript/TSX, JavaScript/JSX, Python, Go — same
  common-path bar.
- Queries in scope: `definition`, `references`, `callers`,
  `implementations`, `dependencies`, `impact`.
- API: **additive** metadata (`confidence`, `resolution_tier`, `notes`);
  existing hit list fields and CLI line format remain stable.
- Library semver: keep `Index::definition` (etc.) returning `Vec<_>`; add
  parallel `*_with_meta` / envelope APIs rather than breaking callers.

## 3. Architecture

Keep the current stack: Tree-sitter plugins → SQLite → shared resolver →
query APIs (library / CLI / MCP / HTTP).

Three layers:

1. **Module identity** — Plugins emit stable, queryable `module_path` values
   and imports that match those paths. Target resolution accepts symbol names,
   module paths, and file paths.
2. **Unified resolve path** — All six queries use
   `resolve_definition_ranked` (tier 1 import / 2 same-module / 3 name-fallback)
   instead of one-off lookup rules.
3. **Result envelope** — Existing hits plus `confidence`, `resolution_tier`,
   and `notes` when answers are fallback or ambiguous.

```text
Source → LanguagePlugin (symbols, refs, imports, impls)
      → SQLite
      → normalize_target + resolve_definition_ranked
      → query (definition | refs | callers | impls | deps | impact)
      → QueryResult { hits..., confidence, resolution_tier, notes }
      → CLI / MCP / HTTP
```

### Known root cause (motivating)

Rust file modules such as `keel/src/mcp/mod.rs` are currently indexed with
`module_path = "crate"`. Querying `dependencies crate::mcp` therefore finds
no files and returns empty even though imports exist. Fixing module identity
is a prerequisite for trustworthy deps/impact/callers.

## 4. Components

### 4.1 Module identity (plugins)

| Language | Common-path rule |
|----------|------------------|
| **Rust** | Derive file-module paths from crate layout: `src/mcp/mod.rs` → `crate::mcp`, `src/mcp/wire.rs` → `crate::mcp::wire`. Inline `mod foo { ... }` unchanged. `mod foo;` declarations associate the name with the child file module. |
| **TS/JS** | Keep path-derived module ids; normalize relative/alias imports so they resolve to the same id the defining file stored. |
| **Python** | Package-style dotted paths from file layout; relative imports resolve against the importing package. |
| **Go** | Keep package name; `main` stays path-qualified. Imports map to the package id used on symbols in that package’s files. |

### 4.2 Target normalization (shared)

Query entrypoints accept:

- bare symbol (`WireFormat`)
- module path (`crate::mcp`, dotted/path module ids)
- file path (`keel/src/mcp/mod.rs`)

and resolve to the defining file set (and optional preferred module) before
deps / impact / callers run.

### 4.3 QueryResult envelope

```text
{
  results: [...existing hits...],
  confidence: "high" | "medium" | "low",
  resolution_tier: 1 | 2 | 3 | "mixed",
  notes: ["..."]
}
```

Confidence heuristic (deterministic):

- **high** — all accepted edges/hits at tier ≤ 2, unique target identity
- **medium** — mix of tier ≤ 2 and tier 3, or multiple definitions expanded
  carefully
- **low** — name-only (tier 3) fallback dominated, or known over-approx

CLI: default human lines unchanged; `--json` includes the envelope.  
MCP / HTTP: same argument shapes; structured payloads gain envelope fields.

### 4.4 Per-query behavior

- **definition / references** — ranked; metadata reflects best/worst tier used.
- **callers** — prefer unique module / import-aware filtering; note when
  falling back to name-only.
- **implementations** — Rust traits as today; other languages only when
  extraction is unambiguous; otherwise empty results + explanatory note.
- **dependencies** — imports of target files + cross-file refs that resolve
  at tier ≤ 2.
- **impact** — same resolve gate; on multi-def names, expand per qualified
  identity and note over-approximation risk.

### 4.5 Common-path fixture pack

One small fixture tree per language covering nested modules, imports,
callers, deps, and impact. Tests assert hits **and** confidence/notes on
hard cases.

## 5. Data flow & error handling

**Indexing:** Unchanged crawl/watch. Corrected `module_path` requires
re-index after upgrade (`rm -rf .keel && keel start`, or query auto-index).
No schema migration required for this pass.

**Query path:**

```text
name/module/file
  → normalize_target → files + optional preferred module
  → ranked resolve (tier 1/2/3)
  → query-specific expansion
  → attach envelope
  → serialize
```

**Hard vs soft:**

- Hard failures (DB/IO): existing `Result` / error responses.
- Soft uncertainty (no hits, ambiguous, tier-3): success with empty/partial
  results, `confidence: low`, and clear `notes`. Do not fail the tool call.

## 6. Testing

- Unit tests for Rust file-module identity (`src/mcp/mod.rs` → `crate::mcp`).
- Unit tests for target normalization (symbol / module / file).
- Unit tests for envelope confidence rules.
- Per-language integration fixtures: nested module + import + call →
  non-empty `dependencies` / precise `callers` / bounded `impact`.
- Regression: existing language and facade tests remain green.
- Manual smoke: `keel dependencies crate::mcp` on this repo returns
  indexed internal modules (not empty).

## 7. Rollout

1. Module identity + target normalization + fixtures (correctness).
2. Wire all queries through shared path; fix deps/callers/impact gaps.
3. Envelope APIs + CLI `--json` + MCP/HTTP metadata.
4. Docs: resolution model, re-index note, confidence semantics.
5. Version bump to 1.2.0 when surface is stable.

## 8. Non-goals reminder

Do not expand into discovery UX, MCP install simplification, or
Stack Graph rewrites in this release. Those remain separate projects.
