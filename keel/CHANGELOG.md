# Changelog

All notable changes to Keel are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [1.2.0] — 2026-08-01

### Added

- Query result envelope (`confidence`, `resolution_tier`, `notes`) via
  `Index::*_with_meta`, MCP tool JSON, and `keel <query> --json`.
- Target normalization: queries accept symbol name, module path, or file path.
- Rust file-module identity from `src/` layout (`src/mcp/mod.rs` → `crate::mcp`).
- Relative import normalization for TypeScript/JavaScript (`./x` → path module id)
  and Python (`.util` → package-qualified module); Go import paths match package
  names by final path segment.
- Common-path integration tests for all five languages (`tests/common_path.rs`).

### Fixed

- `dependencies crate::mcp`-style queries no longer miss file modules that were
  incorrectly indexed as bare `crate`.
- Cross-file `callers` / resolve tier-1 matching for relative JS/TS/Python imports
  and Go module paths like `example.com/app/helper`.

## [1.1.0] — 2026-07-20

### Added

- JavaScript/JSX language plugin (`.js`, `.jsx`, `.mjs`, `.cjs`) with ESM and
  literal CommonJS `require` import extraction.
- Python language plugin (`.py`, `.pyi`) with package-style dotted module paths.
- Mixed-repository indexing across Rust, TypeScript/TSX, JavaScript/JSX,
  Python, and Go in one pass.
- Per-language integration tests under `tests/languages.rs`.
- SHA-256-verified curl installer (`install.sh`) and in-repo Homebrew formula
  (`Formula/keel.rb`).
- GitHub Actions release workflow for macOS/Linux arm64 and x86_64 archives.
- Accuracy benchmark comparing keyword grep vs Keel
  (`scripts/accuracy-benchmark.sh`, `reports/accuracy-benchmark.html`).
- Global daemon (`keel daemon` / `brew services start keel`) plus per-project
  `keel start` / `keel stop` / `keel status` (index + watch into `.keel/`).
- Query-time incremental auto-index for CLI, MCP, and HTTP (disable with
  `--no-auto-index`). Homebrew formula `service` runs `keel daemon`.

### Changed

- Public install path is GitHub binaries / Homebrew (crates.io name `keel` is
  occupied).
- Version bumped to 1.1.0.

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
- `keel watch` reacts to registered language extensions (not Rust-only).
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
- MCP stdio server (`keel mcp`) with code-intelligence tools.
- Language plugin `Registry` dispatch by file extension.

## [0.2.0] — 2026-07-18

### Added

- Schema v2 migration runner (`PRAGMA user_version`).
- Module/import-aware definition resolution and callers.
- Trait `implementations`, module `dependencies`, transitive `impact`.
- Incremental indexing (content hashes) and `keel watch`.
- JSON HTTP API (`keel serve`).

## [0.1.0] — 2026-07-18

### Added

- Initial Rust Tree-sitter indexer and SQLite symbol store.
- CLI: `index`, `definition`, `references`, `callers`.
