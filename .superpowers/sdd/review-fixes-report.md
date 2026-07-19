# Review must-fix report (`fix/review-must-fix`)

## Summary

Implemented all eight must-fix correctness items for SecondBrain. Tests: **64 unit + 16 integration = 80 passed**. `cargo clippy --all-targets -- -D warnings` clean.

## Commits

1. `cc25313` — `fix(lang): path-aware plugins and module identity`
2. `2419885` — `fix(index): relative paths, resilient single-read indexing`
3. `2499eaf` — `fix(graph): tighten dependency and impact resolution`
4. `fix(api,db): WAL, schema guard, API always responds` (tip of branch)

## What was fixed

| # | Fix |
|---|-----|
| 1 | `normalize_path(root, path)`; DB stores root-relative paths |
| 2 | `LanguagePlugin` extract_* take `&Path`; TS `module_path` from path stem; Go `package main` path-based; empty-scope containers use file path |
| 3 | Dep edges from refs only if unique def or top tier ≤ 2 |
| 4 | Impact keyed by `module::name`; expand only when resolve matches target; empty containers use file path at extract time |
| 5 | API always returns JSON (500 on handler error); strip `?`/`#` from `/symbol/{name}`; `PRAGMA journal_mode=WAL` + `busy_timeout=5000` on open |
| 6 | Per-file errors counted in `IndexStats.errors`; index continues; stderr logs |
| 7 | Single read for hash+parse path; invalid UTF-8 → error/skip |
| 8 | `user_version > SCHEMA_VERSION` → `UnsupportedSchema`, no stamp |

## Remaining concerns

- File-path containers (top-level calls) are visited but do not add a `Symbol` to impact results — no synthetic file-level symbol.
- Existing on-disk indexes with absolute paths need a full re-index after upgrade.
- `rust::impl_query` still uses `expect` on a static query (pre-existing).
