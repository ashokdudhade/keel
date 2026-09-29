# AGENTS.md — Keel repo context

Keel is a deterministic, local-first code-intelligence engine (Rust) for AI coding agents.
It indexes a repo with Tree-sitter into on-disk SQLite and answers structural queries
by symbol name — no LLMs, embeddings, or cloud. Released: 1.4.1 (`keel/Cargo.toml`,
tag `v1.4.1`); session work below is implemented but uncommitted — verify with `git status`.

## Repo layout

| Path | What it is |
|---|---|
| `keel/` | The Rust crate (binary + library in one package). All engine work happens here. |
| `keel/src/` | `lib.rs`, `main.rs`, `facade.rs` (`Index`), `cli/`, `db/`, `graph/`, `index/`, `languages/`, `daemon.rs`, `mcp/`, `api/`, `usage.rs` (Insights telemetry) |
| `keel/tests/` | `integration.rs`, `common_path.rs`, `languages.rs`, `service.rs` + inline unit tests |
| `benchmarks/` | `accuracy/` + `impact/` + `references/` fixtures + `gold.json`; `realworld/` gold files + `repos.json` |
| `scripts/` | Benchmark runners, `keel-mcp.sh`, `mcp-trust-smoke.py`, `test-install.sh` |
| `reports/` | Generated benchmark HTML/JSON + bake-off notes (outputs, not sources) |
| `website/` | Marketing site (React + Vite + TS); GitHub Pages builds `website/dist` |
| `site/` | Legacy static site, reference only |
| `Formula/keel.rb` | Homebrew formula (checksums filled by Release workflow) |
| `install.sh` | SHA-256-verified curl installer (macOS/Linux, no native Windows — WSL2) |
| `.github/workflows/` | `ci.yml`, `release.yml`, `tag-release.yml`, `pages.yml` |
| `.cursor/rules/keel-mcp.mdc` | Cursor rule: prefer Keel MCP for structural queries, read trust envelope |
| `README.md` | User docs (install/upgrade/MCP/CLI); `keel/README.md` = library + contributor docs |
| `Keel_Proposal.md`, `keel_cursorrules.md` | Original vision + v0.1 scaffolding guide (historical, still useful for conventions) |
| `docs/superpowers/` | Design specs and plans (e.g. query-trust design) |

## Architecture (crate `keel`, edition 2021, Rust stable)

- **Public API:** `Index` in `keel/src/facade.rs` — `open` / `open_in_memory`,
  `index_path` / `index_path_with`, plus `definition`, `references`, `callers`,
  `implementations`, `dependencies`, `impact` and `*_with_meta` variants.
  Prefer it over reaching into `db`/`graph`/`index` directly.
- **Languages:** `LanguagePlugin` trait + `Registry` (`languages/mod.rs`).
  Built-ins: Rust (`.rs`), TS/TSX (`.ts/.tsx/.mts/.cts`), JS/JSX
  (`.js/.jsx/.mjs/.cjs`), Python (`.py/.pyi`), Go (`.go`). Mixed monorepos index
  in one pass; external crates add plugins via `Registry::empty` + `register`
  (or extend `with_defaults`).
  Plugins must be `Sync` (Rayon workers). TS/JS register via `register()` fns
  (multiple plugins per file family); others are single structs.
- **Indexing:** `index/worker.rs` collects files with the `ignore` crate
  (respects `.gitignore` even without `.git`, plus `.keelignore`, sorted
  for determinism), hashes
  (SHA-256) + parses in parallel with Rayon, then `index/mod.rs` persists in one
  transaction. Incremental: unchanged hashes skipped, missing files deleted,
  per-file failures counted in `IndexStats.errors` without aborting; files
  that parse with syntax errors still index but warn per file and count in
  `IndexStats.syntax_errors` (`LanguagePlugin::has_syntax_errors`).
  Parsing runs on a dedicated 64 MiB-stack rayon pool, and every
  extraction walk enforces `MAX_WALK_DEPTH` (1024; real max observed: 89):
  deeper files fail loudly per file instead of overflowing a stack.
- **Storage:** `rusqlite` bundled SQLite, `PRAGMA user_version = 6`
  (`db/schema.rs` migrates v0–v5 → v6 idempotently in one transaction;
  v6 adds the `symbols.container` column + index and forces one rebuild).
  Tables: `files`, `symbols` (+`module_path`, +`container`), `"references"` (+`kind`,
  +`container`, +`qualifier`), `imports`, `impls`, `meta` (writer stamps).
  `INDEX_FORMAT_VERSION` (=2) tracks content semantics separately from schema:
  the indexer auto-rebuilds stale content; reads via facade/HTTP refuse with
  `KeelError::StaleIndex` when bypassing auto-index. `meta` also holds
  `keel_version` + `last_indexed` for the Insights portal.
- **Resolution:** `graph/resolve.rs` tiers — 1: exact `module::name` via importer
  row, 2: same-module, 3: name-only fallback; ordered by `(path, line, col)`
  within a tier. `graph/target.rs` normalizes a query to symbol/module/file.
  `graph/deps.rs` requires tier ≤ 2 evidence for edges (no name-only fallback —
  fabricated edges); `graph/impact.rs` keeps the looser candidate rule via
  `acceptable_top_match`. `suggest_names` powers did-you-mean miss recovery.
  `Dependency.external` marks unindexed (stdlib/third-party) modules.
- **Trust envelope:** `graph/query_result.rs` — every `*_with_meta`, MCP payload,
  and `keel <q> --json` returns `results` + `confidence` (high/medium/low) +
  `resolution_tier` (0 empty/n-a, 1, 2, 3, mixed) + `notes`. Empty + "No matching
  symbols found" is a confident miss (high/0), not a failure. Non-empty `impact`
  is always a candidate blast radius (medium/low) — never high.
- **Module identity per language:** Rust `crate::…` derived from `src/` layout
  (`src/mcp/mod.rs` → `crate::mcp`); TS/JS path-based (`src/auth/service`),
  relative imports (`./x`) normalized to the same ids; Python dotted packages,
  relative (`.util`) normalized; Go package names, `package main` path-qualified,
  import paths matched by final segment. `implementations` covers Rust traits,
  TS interfaces/`extends` (declarations and class expressions),
  Python/JS base classes, and explicit Go `var _ I = T{}` assertions
  (structural Go matches are not inferred).
- **Daemon:** `daemon.rs` — global control plane on `127.0.0.1:7646`
  (`KEEL_DAEMON_PORT`, state in `~/.keel/daemon/` aka `KEEL_HOME`, registry
  `projects.json`). `keel start` indexes into `<project>/.keel/index.db` and
  spawns `keel watch <root>` (logs to `.keel/watch.log`); `stop`/`status` manage it.
- **MCP:** `mcp/mod.rs` — stdio JSON-RPC 2.0, accepts NDJSON (Cursor 2025-11+)
  and Content-Length framing; stdout is protocol-only, logs to stderr. Ten
  tools: `definition`, `references`, `callers`, `implementations`,
  `dependencies`, `dependents`, `impact`, `outline`, `search`, `unused`
  (+`index`). The eight hit tools take `limit` and `preview`
  (source line per hit).
  `definition`/`references`/`callers`/`impact`/`implementations`
  accept optional `module` or qualified `name` (`crate::mcp::serve`); when both
  are passed, `module` wins and `name` is stripped to bare symbol.
  `definition`/`search` also take member form (`Type.member` and
  single-segment `Type::member`; `definition` tries the module reading
  of `::` first, `search` is case-insensitive).
- **Index resolution** (`cli/commands.rs::resolve_index_db`, MCP): 1. `KEEL_INDEX_DB`
  if set → 2. walk up from cwd for `.keel/index.db` → 3. daemon registry
  (project containing cwd, else sole registered index; never guess among many)
  → 4. `cwd/.keel/index.db`. `KEEL_MCP_DEBUG=1` prints the choice on stderr.
  CLI queries use `query_root` (step 2 then 4 only): subdir queries read the
  ancestor index; explicit `index`/`watch` paths and `KEEL_INDEX_DB`/registry
  keep their scope.
  File targets are suffix-tolerant (`graph::target::resolve_file_target`,
  shared by `dependents`/`outline`/`unused`): exact → cleaned → unique
  suffix, so `../../x.py`, absolute paths, and bare basenames from subdirs
  resolve; runs after module/dir steps, `/`-boundary required, ambiguous
  tails miss honestly with suggestions, extensionless identifiers pass
  through to symbol resolution.
- **HTTP:** `api/mod.rs` via `keel serve` on `127.0.0.1:7645`:
  `GET /health`, `GET /symbol/{name}[?limit=N]`, `GET /outline/{path}[?limit=N]`,
  `GET /search/{pattern}[?limit=N]`, `GET /impact/{name}[?module=M]`,
  `GET /dependents/{target}[?limit=N]`, `GET /insights` (embedded dashboard),
  `GET /api/insights` (JSON).
  `keel insights [--port N]` auto-starts a
  background `serve` (free-port fallback, reuse via `.keel/insights.port`,
  adopts a hand-started serve for the same project, alias `insight`,
  `KEEL_NO_BROWSER=1` skips browser). CLI output is `path:line:col` (1-based),
  tab-separated, deterministic; all eight hit commands
  (definition/references/callers/implementations/impact/search/outline/unused) share
  hit rows via `cli::commands::format_{symbol,reference,impl}_hit`
  (`path:line:col⇥kind⇥name`; implementations print `⇥type`)
  (`implementations`/`dependencies`/`dependents` print module-oriented rows).
  `--preview` adds the source line per hit (indented row; `preview` field in
  JSON/`--json`, absent when off) via `cli::commands::PreviewCache`.
- **Insights telemetry:** `usage.rs` appends one JSON object per query to
  `.keel/usage.jsonl` (5 MiB rotation, `KEEL_NO_USAGE_LOG=1` opt-out).
  Emission sites: 9 CLI branches, 9 MCP arms, 5 HTTP routes
  (`/symbol` aggregate, `/outline`, `/search`, `/impact`, `/dependents`);
  in-memory DBs skip. Reference kinds: Call/Macro/Method/Type/Path/**Value** (bare
  identifiers in call args, receivers, return/throw/yield, operators,
  conditions, loop iterables, assignment RHS, subscripts, template/JSX/f-string
  interpolation, Rust format-string holes, struct-literal keys (Go/Rust),
  TS/JS defaults/field initializers/arrow bodies, Rust static/const
  initializers, decorator/derive lists, Rust macro args, member reads
  with receiver qualifiers — all seven grammars. Still excluded by
  design: imports, bindings/writes, labels,
  dict/object-literal keys, other string contents, `macro_rules!`
  templates). Path refs store their qualifier
  (`mcp` in `mcp::serve`); Method refs store receiver text (`db` in
  `db.get()`) — all five languages — so module-scoped callers can break
  import-tier ties; undecided ties stay dropped with an honest note.
  Schema v6, index format 3 (migrations via `schema::initialize`). Member
  symbols record their enclosing type in `symbols.container`, so
  `definition` accepts `Type.member` (and single-segment `Type::member`
  as a fallback when the module reading misses).
- **Onboarding commands:** `keel init` (index now + MCP config print; no daemon needed),
  `keel doctor` (version/daemon/project/MCP/index checks; flags corrupt DBs as unreadable), `keel daemon-stop`,
  `--version`. `keel index/watch <path>` write to `<path>/.keel/`; daemon pids
  are validated before signaling (`valid_pid`).
- **Errors:** `thiserror::KeelError` in library, `anyhow` at `main.rs` boundary.

## Build / test / dev

```bash
cd keel
cargo test            # unit + integration tests (CI runs this + release build)
cargo build --release # binary at keel/target/release/keel
cargo clippy --all-targets -- -D warnings   # Release workflow gates on this
cargo install --path ./keel                 # contributor install from source
```

- Needs a C toolchain for bundled SQLite (`xcode-select --install` on macOS;
  `build-essential` on Debian/Ubuntu). macOS license quirk:
  `DEVELOPER_DIR=/Library/Developer/CommandLineTools cargo build --release`.
- MCP smoke: `keel index . && KEEL_BIN=<path> python3 scripts/mcp-trust-smoke.py`
  (CI's `mcp-trust-smoke` job does exactly this).
- Benchmarks: `scripts/accuracy-benchmark.sh`, `scripts/realworld-accuracy-benchmark.sh`,
  `scripts/impact-benchmark.sh` (transitive impact recall, 5 languages, no grep baseline),
  `scripts/references-benchmark.sh` (module-scoped references vs whole-word grep).
  All three core gates hold F1 1.0 (impact 52 queries, accuracy 29 incl. 9 member-qualified, references 12).
- Website: `cd website && npm ci && npm run build` (Node 20).
- Reset a project index: `rm -rf .keel && keel start` (or `keel index .`).

## Release process (do not improvise)

- Pushing to `main` does **not** release. Cut releases via GitHub Actions →
  **Tag and release** (patch/minor/major or exact version) → tags `vX.Y.Z` →
  triggers **Release** (runs tests + clippy, builds 4 targets
  macOS/Linux × arm64/x86_64, publishes archives + `SHA256SUMS`, updates
  `Formula/keel.rb` checksums).
- After releases that change module identity or index format, users must
  `rm -rf .keel && keel start` and refresh MCP in the IDE.
- The crates.io name `keel` is taken — never `cargo install keel`; use GitHub
  binaries / Homebrew / `cargo install --path`.

## Conventions for edits

- No `.unwrap()`/`.expect()` outside tests; propagate with `?`.
- No `unsafe`. Strong types for domain concepts (`PathBuf` for paths).
- Rustdoc (`///`) on public items; explain non-obvious Tree-sitter queries.
- Keep language specifics behind `LanguagePlugin`; `db`/`index`/`graph` stay
  language-agnostic. New languages: new module in `languages/`, register in
  `Registry::with_defaults`, add extensions + tests in `keel/tests/languages.rs`.
- SQLite: schema changes go through `schema.rs` migration runner, bump
  `SCHEMA_VERSION`, keep idempotent + transactional; `"references"` is a reserved
  word — always quote it.
- MCP/CLI/HTTP contracts are semver-stable: keep output shapes, tool names/args,
  and the `confidence`/`resolution_tier`/`notes` envelope backward compatible.
- Update `keel/CHANGELOG.md` (Keep a Changelog) for user-visible changes.
