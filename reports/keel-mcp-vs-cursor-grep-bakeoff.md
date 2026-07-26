# Keel MCP vs Cursor Grep — agent bake-off

**Date:** 2026-07-25  
**Workspace:** `second-brain` / Keel crate  
**Agent:** this Cursor chat, with `user-keel` MCP connected  
**Method:** Fixed structural-search tasks. For each task, call Keel MCP first, then the closest Cursor built-in (`Grep`). Score usefulness for an agent that needs a precise answer, not a raw text dump.

## Tool-call counts (this bake-off)

| Tool surface | Calls | Role |
|--------------|------:|------|
| **Keel MCP** (`definition`, `references`, `callers`, `implementations`, `dependencies`, `impact`) | **10** | Primary structural search |
| **Cursor Grep** | **7** | Parity baseline only |
| Cursor Glob / Read / Shell (for search) | **0** | Not needed once MCP returned |

Keel was used **~1.4× as often as Grep** in this controlled pass. Grep calls existed only to compare; for normal agent work after MCP works, Grep would often be skipped on these tasks.

## Task results

| # | Task | Keel tool | Keel outcome | Grep outcome | Winner |
|---|------|-----------|--------------|--------------|--------|
| 1 | Where is `Index` defined? | `definition` | `facade.rs:20` struct + impl (2) | Matches `Index` and `IndexStats` | **Keel** (no `IndexStats` noise) |
| 2 | What implements `LanguagePlugin`? | `implementations` | 8 impls (Rust/TS/JS/Python/Go/Toy) | 8 `impl LanguagePlugin for` lines | Tie |
| 3 | Who references `read_message`? | `references` | 3 call sites | 4 lines (includes definition) | **Keel** (refs ≠ def) |
| 4 | Who calls `run_mcp`? | `callers` | `main.rs:121` | Def + call (2) | **Keel** (caller semantics) |
| 5 | Impact of changing `WireFormat` | `impact` | 14 related symbols | Text hits only (~15 lines, no graph) | **Keel** (Grep cannot answer) |
| 6 | Dependencies of `crate::mcp` | `dependencies` | `[]` (gap) | N/A as module graph | Neither (Keel empty) |
| 7 | Where is `encode_message_with`? | `definition` | `mcp/mod.rs:44` | Would work with exact name | Tie / Keel faster |
| 8 | Definitions of `serve` | `definition` | api + mcp (2 overloads) | Same 2 `pub fn serve` | Tie |
| 9 | References to `initialize_result` | `references` | 2 call sites | 3 lines (includes `fn`) | **Keel** |
| 10 | Impact of `Registry` | `impact` | Large transitive set (70+) | Would need many greps + manual graph | **Keel** |

**Scorecard:** Keel wins **6**, tie **3**, neither **1**.

## When this agent prefers each tool

**Prefer Keel MCP when the question is structural**
- “Where is X defined?”
- “Who calls / references X?”
- “What implements trait T?”
- “What breaks if I change X?” (`impact`)

**Prefer Cursor Grep when**
- Searching prose, comments, config, or unknown substrings
- Pattern / regex exploration (`Content-Length|Ndjson`)
- Keel returns empty (e.g. `dependencies` for `crate::mcp` in this run)
- The symbol name is unknown and must be discovered from text

## Qualitative notes

1. **MCP is usable end-to-end** after the NDJSON + `KEEL_INDEX_DB` fix; all 10 Keel calls returned JSON without timeouts.
2. **Precision > recall for agents:** Grep often includes the definition line; Keel `references` / `callers` separate those concerns.
3. **Impact is the clearest differentiator:** no single Grep call replaces transitive impact.
4. **Gap to watch:** `dependencies` returned empty for `crate::mcp` — agent must fall back to Grep/Read for module wiring until that query is stronger.
5. **Ambiguous names (`serve`, `Index`):** Keel returns multiple structured hits; Grep needs carefully crafted patterns to avoid cousins (`IndexStats`).

## Recommendation

For agent workflows on an indexed Keel project, default to MCP for named-symbol questions, keep Grep as fallback for text/regex and Keel misses. In this bake-off that policy would have produced roughly **10 Keel + 1–2 Grep** calls instead of **10 + 7**.
