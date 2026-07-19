# Changelog

All notable changes to SecondBrain are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [1.0.0] — 2026-07-19

### Added

- Stable library facade: `Index` with `open` / `open_in_memory`, `index_path` /
  `index_path_with`, and query methods (`definition`, `references`, `callers`,
  `implementations`, `dependencies`, `impact`).
- Multi-language monorepo crawl: one index pass collects all registry extensions
  (Rust + TypeScript/TSX + Go) in a single tree; integration coverage for mixed
  repos.
- Community plugin surface: `Registry::empty`, `Registry::register`,
  `index_repository_with`, and `Index::index_path_with`.
- `sb watch` reacts to registered language extensions (not Rust-only).
- Rust `impl` extraction uses a `OnceLock`-cached Tree-sitter `Query`.

### Changed

- Crate version and public API marked stable for 1.0 consumers.
- README documents library API, monorepos, plugins, MCP, HTTP, and CLI.

### Notes from 0.x → 1.0

No intentional breaking changes to existing CLI commands or SQLite schema
(`user_version` remains 2). New APIs are additive.

## [0.3.0] — 2026-07-19

### Added

- TypeScript/TSX language plugin (`.ts`, `.tsx`, `.mts`, `.cts`).
- Go language plugin (`.go`).
- MCP stdio server (`sb mcp`) with code-intelligence tools.
- Language plugin `Registry` dispatch by file extension.

## [0.2.0] — 2026-07-18

### Added

- Schema v2 migration runner (`PRAGMA user_version`).
- Module/import-aware definition resolution and callers.
- Trait `implementations`, module `dependencies`, transitive `impact`.
- Incremental indexing (content hashes) and `sb watch`.
- JSON HTTP API (`sb serve`).

## [0.1.0] — 2026-07-18

### Added

- Initial Rust Tree-sitter indexer and SQLite symbol store.
- CLI: `index`, `definition`, `references`, `callers`.
