# Changelog

All notable changes to Keel are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `index` remembers files that fail with a content hash (invalid UTF-8,
  extractor failures): unchanged bad files skip on later passes instead
  of re-printing the same error on every query. Editing the file retries
  it; unreadable files (no hash) still retry each pass.
- Schema v5 adds `file_id` indexes on all four row tables (auto-migrated
  in place, no reindex), and `impact` memoizes its BFS lookups per run:
  worst-case transitive impact on a 600k-reference repo drops from
  minutes to ~4s (indexes alone give 14× on the same build).
- `index` caps per-file syntax/error diagnostics at 10 lines per pass,
  folding the rest into a summary count (large repos printed hundreds).
- New `unused` dead-code sweep: `keel unused [file|module|dir]` and the
  MCP `unused` tool list functions/methods with no recorded references
  (Low-confidence candidates with blind-spot notes; `main`,
  constructors, test files, inline test modules, Go `init` /
  `Test*` / `Benchmark*` / `Example*` / `Fuzz*`, React `render`, and
  Python dunders exempt). `--transitive` / `transitive: true` also
  flags functions referenced only from other candidates.
- `implementations` joins the preview family: CLI `--preview` (text and
  JSON), MCP `preview` argument, and HTTP `?preview=1` on `/symbol/`
  implementation rows all show the impl source line.
- `doctor` detects a corrupt (non-SQLite) index database and reports it
  as unreadable with a delete-and-re-index fix, instead of misreporting
  a stale format version.
- Rust method calls record the receiver as qualifier (`c` in `c.port()`),
  matching the other four languages, so module-scoped `references` and
  `callers` attribute method sites across same-name definitions.
- New references benchmark (`scripts/references-benchmark.sh`, CI-gated):
  Keel module-scoped references vs whole-word grep over 12 multi-language
  queries (Keel F1 1.0, grep F1 ~0.16).
- CI enforces `cargo clippy --all-targets -- -D warnings` and both
  benchmark gates (impact and accuracy F1 must hold at 1.0); the
  benchmark scripts now exit nonzero on any false positive/negative.
- Namespaced heritage counts in TypeScript/JavaScript: `extends ns.Base`
  and `implements ns.IFace` (generics included) record by final segment,
  with module attribution via the file's imports; call-form heritage
  (`extends mixin(Base)`) stays out.
- Python dynamic loaders record module edges:
  `importlib.import_module("pkg.m")` and `__import__("pkg.m")` (aliased
  when simply assigned); interpolated and non-literal targets stay out.
- TypeScript `export = Name` and `export default Name` (JS too) read the
  exported name as a value reference; declarations and specifier lists
  stay out.
- Dynamic `import("…")` records a module edge in TypeScript/JavaScript
  (aliased when bound: `const m = await import("…")`); non-string
  targets stay out and declarator values emit once.
- TypeScript `const util = require("./util")` records the same
  namespace edge JavaScript already had.
- Python module-level variables are indexed (`variable`, `Const` for
  ALL_CAPS, annotated/chained included); destructured names stay out and
  augmented assignment no longer claims a definition.
- Rust `union` items are indexed as `Struct` symbols.
- Rust `static` and `const` initializers read as value references
  (`static L: T = BASE`); the bound name stays a definition.
- TypeScript/JavaScript declaration-site reads: parameter defaults
  (`(a = d)`, `(a: T = d)`), destructuring defaults (`{a = d}`, `[a = d]`,
  incl. assignment targets), class field initializers (`x = v`), and
  concise arrow bodies (`=> x`) read as value references; bound
  patterns, field names, and write targets stay out.
- Struct-literal keys read as value references: Go `Point{X: v}` and Rust
  `Point { x: v }` keys name declared fields, and Go map variable keys
  (`m[k]`) read as variables; string/composite keys, Rust patterns, and
  declarations stay out.
- Rust format-string holes (`format!("hello {name}")`, `println!`,
  `write!`, `assert_eq!` with message, and the rest of the std format
  family) read as value references with exact positions; escapes
  (`{{`/`}}`), positional holes, compared values, and non-format macro
  strings stay out.
- Namespace-qualified calls now resolve in `impact`: `ns.fn()` through
  `import * as ns` (all languages with namespace imports) attributes to
  the target module when the qualifier resolves through an import row,
  while unknown receivers (`client.get()`) with unrelated imports still
  stay out via the tier-3 method gate.
- Legacy TypeScript `import util = require("./util")` records a
  whole-module import edge aliased to the local name, so
  `dependencies`/`impact` see through the require form.
- Go interface method specs (`Do(x int) int` in `type Doer interface`)
  are indexed as `Function` symbols, so `outline`/`definition` find
  interface members; embedded interface names stay uses, never defs.
- Bare identifiers in JSX/TSX expressions (`<W timeout={limit}>`) are
  indexed as value reads; attribute names stay markup, never reads.
- Rust macro arguments are no longer opaque: bare identifiers in macro
  call token trees (`vec![seed]`, `assert_eq!(a, b)`) and in attribute
  arguments (`#[derive(Debug, Clone)]`) are indexed as value reads, so
  `references`/`impact` see through `vec!`/`format!`/derives.
  `macro_rules!` bodies still stay out (they declare templates, never
  uses), as do string contents (`format!("{x}")`) and scoped paths in
  macros.
- `implementations` now covers TS/JS class expressions
  (`const X = class implements I`, `module.exports.Anon = class extends B`),
  named by their declarator, assignment target, or own `class Y` name.
- `implementations` now covers explicit Go assertions
  (`var _ io.Reader = MyReader{}`, `&T{}`, `(*T)(nil)`, generic
  interfaces); named vars, constructor calls, and bare values prove
  nothing and stay out, as does structural method-set inference.
- Indexing honors an optional `.keelignore` file (gitignore syntax) for
  excluding generated or checked-in code without touching version control.
- `--preview` on the six hit commands (definition/references/callers/
  impact/search/outline) shows the source line of each hit: an indented
  continuation row in text output (hit rows still match "no leading
  whitespace"), a `preview` field per hit under `--json`. Lines truncate
  at 200 characters; stale rows preview as nothing, never an error.
- Same previews on the other surfaces: a `preview` argument on the six
  hit-list MCP tools, and `?preview=1` on `GET /symbol`, `/outline`,
  `/search`, and `/impact` (hit lists only, matching the CLI scope).
- `implementations` now records generic and scoped Rust impls
  (`impl<T> Store<T> for Mem<T>`, `impl m::Logger for Svc`) by outer
  name, matching the bare-name lookup; tuples/arrays/`dyn` still stay out.
- `implementations` now records Python generic-alias bases
  (`class Repo(Base[int])` counts as `Base`); dotted bases stay out per
  the pinned bare-bases rule.
- TypeScript type alias bodies are no longer opaque: named types in
  conditionals, unions, template holes, and function types
  (`T extends Array<infer U> ? U : B`) are indexed as type reads, with
  `infer`/parameter bindings staying out and the alias itself scoping
  its body for `impact`.
- TS/JS `export { name }` specifiers (including `export type` and
  aliased/re-export forms) read the local name, so renames stay
  complete; export aliases name the export and stay out.

### Fixed

- Python `except E as e` (and `except (A, B) as e`) no longer indexes the
  `as` alias as a type reference; the handler parses as an `as_pattern`
  whose alias binds a fresh local. The covering test asserted the wrong
  line and now pins the alias line, the absence of any type row, and the
  later value read.
- Files starting with a UTF-8 BOM no longer report a spurious syntax
  error; the indexer blanks the 3 BOM bytes to spaces before parsing, so
  reported lines and columns stay raw-file exact.

- `dependencies` and `dependents` accept `--limit` (1-100000, default 500)
  on the CLI, a `limit` argument on the MCP tools, and `?limit=N` on
  `GET /dependents/{target}`; capped responses carry the true total in a
  note, matching `references`/`callers`/`impact`.
- `implementations` accepts `--limit` on the CLI and a `limit` argument on
  the MCP tool, with the same capped-response note as the other hit lists.
- `definition` accepts `--limit` on the CLI and a `limit` argument on the
  MCP tool, so colliding names (dozens of `parse`/`new` defs in a monorepo)
  cap with the true total in a note instead of flooding.
- `outline` accepts `--limit` on the CLI, a `limit` argument on the MCP
  tool, and `?limit=N` on `GET /outline/{path}`; subtree outlines cap with
  the true total in a note.
- `GET /symbol/{name}[?limit=N]` caps each aggregate list (default 500)
  with per-list notes naming the list and true total (`definition: showing
  first 1 of 3 matches; …`) plus a `notes` array on the payload, so hot
  symbols stop flooding HTTP consumers.
- Indexing surfaces per-file syntax errors instead of silently ingesting
  partial trees: each offending file warns on stderr (`index warning:
  bad.py: syntax errors; symbols may be incomplete`), counts in the new
  `IndexStats.syntax_errors` (CLI text/`--json`, MCP `index`, daemon/watch
  logs, auto-index notices), and extraction continues. New
  `LanguagePlugin::has_syntax_errors` (default `false`) keeps third-party
  plugins source-compatible.
- TypeScript/JavaScript re-exports are import edges: `export * from`,
  `export { x } from`, and `export * as ns from` record the source module,
  so `dependencies`/`dependents` cover barrel files (plain `export …`
  declarations without a source are untouched).
- Module-level variables are definitions: top-level TypeScript/JavaScript
  `const` (`const` kind; `let`/`var` are `variable`), including `export …`
  and `declare …` forms, with function locals and destructured names
  excluded; Rust `static` items (`const` kind, as the enum always
  anticipated); Go top-level `var` specs. TypeScript also gains the
  JavaScript walker's bound arrow-function symbols (`const f = () => …`
  is `Function`, emitted once).
- TypeScript/JavaScript value-position reads are references: `return`/
  `throw`, initializers, binary/unary/ternary operands, arrays, spreads,
  `await`/`yield`, parenthesized/sequence expressions, non-null assertions
  (TS), assignment RHS (plus compound-assignment/update targets),
  object-literal values (generalized from call args), computed `a[b]`
  sides, `for…of/in` iterables, `case` values, enum initializers, and cast
  value sides. Writes (plain-assignment LHS, loop targets), bindings, and
  property keys stay excluded; each site emits exactly once. Impact
  benchmark grows to 27 queries (F1 1.0).
- Python value-position reads are references too: `return`/`raise`/
  `assert`, assignment RHS (plus compound targets), all operator kinds,
  subscripts/slices, `for`/`while`/`if` conditions and iterables, list/
  tuple/set/dict displays (values only), comprehensions (bodies,
  iterables, `if` filters), `yield`/`await`, lambdas, walrus values,
  `with` items (unwrapping aliased `as_pattern`), keyword values,
  parameter defaults, `match` subjects, and bare/`*`/`**`/parenthesized
  expressions. Tuples inside `except (A, B)` stay owned by the handler
  arm (no Type+Value double emit). Impact benchmark grows to 28 queries
  (F1 1.0).
- Rust value-position reads are references: block tails, `return`,
  binary/unary operands, `if`/`while` conditions, `for` iterables,
  `match` scrutinees and bare arm bodies, `let` initializers,
  assignment RHS (plus compound-assignment targets; plain LHS stays a
  write), arrays, tuples, borrow/deref/cast value sides, index
  expressions, ranges, struct-literal field values, bare closure
  bodies, `break` values, and call arguments. Each site emits exactly
  once; patterns, labels, and path roots stay excluded. Qualified path
  uses (`m::C`, `crate::m::C`, type-position `m::S`) read their final
  segment with the qualifier attached; struct-literal type paths are
  `type` uses (the definition-name guard now only matches genuine
  definition parents); bodiless trait declarations are `function`
  symbols (metavariables excluded). Impact benchmark grows to 30
  queries (F1 1.0).
- JSX/TSX element usages are `call` references: `<Widget/>` resolves to the
  `Widget` component, `<Foo.Bar/>` keeps the `Foo` qualifier like a member
  call, and lowercase host tags (`<div/>`) are skipped. `references`,
  `callers`, and `impact` now cover component render sites.
- Rust `macro_rules!` definitions are indexed (kind `macro`), so `definition`
  finds a macro and `callers`/`references` already-recorded `macro`
  invocations resolve to it.
- Python decorator applications are references scoped to the decorated
  definition: `@auth` on `login` is a `call` of `auth` attributed to
  `login`, and `@app.route("/x")` attributes `route` (qualified `app`)
  there too, so `impact` covers decorator use sites.
- TypeScript/JavaScript decorators work the same way: `@Logged` on
  `Service` (including `@Logged export class Service`) is a `call` of
  `Logged` attributed to `Service`; call-form `@Route("/x")` attributes
  under the decorated name too.
- Go type uses are `type` references: annotations, pointer/result types,
  assertions (`v.(*Widget)`), and composite literals (`Widget{}`) resolve
  to the type; `pkg.Type` keeps the package qualifier, definitions
  (`type_spec.name`) never emit, and conversions (`Widget(x)`) stay
  `call`-only with no double row.
- JavaScript/TypeScript heritage clauses are `type` references:
  `extends Base`, `implements Shape`, interface `extends`, and generic
  bounds/defaults (`<T extends Shape>`) resolve to the named type and
  attribute to the subclass/interface, so `impact` covers inheritance.
  Call-form heritage (`extends mixin(Base)`) keeps call rules with no
  double row. Interfaces now scope their contents like classes.
- JavaScript/TypeScript shorthand properties (`{ handler }`) are `value`
  references; destructuring patterns stay bindings, never reads.
- Python `except` handlers name `type` references (`except AppError`,
  tuples, qualified `except mod.Err`), so `impact` covers catching code;
  the `as` alias stays a binding and call-form handlers keep call rules.
- TypeScript `as`/`satisfies` cast targets are `type` references.
- JavaScript/TypeScript template interpolations (`` `${name}` ``) are
  `value` reads for directly embedded identifiers.
- Rust struct-literal shorthand (`Point { x }`) reads the variable as a
  `value` reference; pattern shorthand stays a binding.
- Python f-string interpolations (`f"hi {name}"`) are `value` reads for
  directly embedded identifiers.
- Value-position member reads record their receiver in all five languages
  (`items` in `items.length` / `items.Count` / `cfg.port`), so
  `references <variable>` finds attribute/field/selector reads. Chains
  read their base once (`a.b.c()` reads `a`); call/new callees, JSX tags,
  heritage clauses, annotations, and `except` handler types stay owned by
  their own arms with no double rows.
- TypeScript constructor calls (`new Helper()`) resolve like calls,
  matching JavaScript.
- Python no longer records `self`/`cls` receivers as `value` references
  (calls or reads), removing a large noise source.
- Python `match` value and class patterns (`case Color.RED`,
  `case Point(x=0)`) are `value` reads; capture patterns stay bindings
  and import paths are excluded.
- Bodiless declarations are definitions: TypeScript abstract methods,
  interface members, and function overloads, plus Rust trait method
  declarations (`fn get(&self);`, metavariables excluded).
- Same-crate Rust `use` paths normalize to `crate::` roots at index time:
  `use m::store::Mem as Store` inside crate `m` (crate name read from the
  nearest `Cargo.toml`, `[lib]` renames and `-`→`_` honored) records
  `crate::store::Mem`, so aliases, qualified calls, and re-exports
  resolve against the `crate::`-rooted definitions. External crates pass
  through untouched.
- HTTP `?limit=` mistakes are visible: unparseable or clamped values on
  `/search` and `/impact` add a `notes` warning naming the fallback
  (`invalid limit 'bogus', using 500`), instead of failing silently.
- `dependents` no longer drops evidence from files that define no symbols:
  a top-level-only script or barrel that imports the target now lists
  under its path-derived module (same identity the extractor uses), with
  the evidence file attached. Go keeps today's behavior (no path-derived
  identity for package clauses).
- `impact` bridges file-scope uses: resolved top-level uses with no
  enclosing symbol add a note naming their files
  (`Also used at file scope in main.py ...; see references helper`) on
  every surface, and the CLI's empty header reads `No impacted symbols`
  when such uses exist instead of claiming nothing was found.
- Module targets resolve files that define no symbols: `dependencies main`
  now finds `main.py`'s imports via its path-derived module identity
  (gap-fill only — symbol-bearing files resolve exactly as before), and
  `outline` accepts such modules too. Shared fallback helper lives in
  `languages` for both target resolution and dependents.

- `keel insights-stop`: stop this project's background Insights server
  (cleans stale markers; never kills a hand-started `keel serve`).
- Rust bare-identifier call arguments are `value` references
  (`handler` in `spawn(handler)`), matching Python/TypeScript/JavaScript.
- Go bare-identifier call arguments are `value` references too, completing
  all five languages.
- Method receivers are `value` references in all five languages
  (`svc` in `svc.get()`), so `references <variable>` finds use sites.
- `keel outline <file>` + MCP `outline` tool: symbols defined in a file in
  source order (accepts `./`-prefixed and absolute paths, suggests near
  matches on miss, names indexed-but-empty files honestly).
- HTTP `GET /outline/{path}`: same outline payload with the
  `confidence`/`notes` trust envelope (completes CLI+MCP+HTTP coverage).
- `keel search <pattern> [--limit N]` + MCP `search` tool + HTTP
  `GET /search/{pattern}[?limit=N]`: case-insensitive substring search over
  symbol names (exact matches rank first, limit 1-200 default 50,
  truncation and partial-only notes, `%`/`_` treated literally).
- `keel dependents <name|module|file>` + MCP `dependents` tool: reverse
  dependencies — modules importing the target or referencing its symbols
  (same tier ≤ 2 evidence bar as forward edges, never self-lists).
- `--module M` flag for `keel definition|references|callers|impact` (CLI/MCP
  parity; module is also recorded in usage telemetry).
- Honest empty notes for callers/references when definitions match but no
  sites resolve (replaces the misleading "No matching symbols found.").
- Module-scoped callers/references no longer attribute qualified (`path::f`)
  call sites at high confidence when two modules tie for the top rank —
  qualifiers are dropped at extraction, so tied sites are unattributable
  (reported with an explanatory note instead of a guessed winner).
- Qualified call sites now keep their qualifier (Rust path prefix in
  `mcp::serve`; receiver text in `db.get()` for Python/TypeScript/JavaScript/
  Go), so module-scoped callers/references attribute each site to the module
  its qualifier selects — even when several modules define the name and the
  caller imports them all. Schema v4 + index format 2 (one automatic rebuild
  on upgrade; legacy rows stay honestly unattributable).
- Impact expansion uses stored qualifiers to decide module ties the same way,
  fixing both directions: qualified sites no longer leak into the wrong
  module's blast radius, nor vanish from the right one.
- Reference JSON payloads (CLI `--json`, MCP, HTTP) carry the stored
  `qualifier`, so clients can tell `mcp::serve` from `api::serve` without
  opening the file (additive field; CLI text format unchanged).
- Insights dashboard Search card: interactive symbol search (same `/search`
  endpoint, still fully local and offline).
- References/callers include uses through import aliases (`serveA()` for
  `import { serve as serveA }`), attributed to the exact mapped module at
  tier 1 (aliases shadowed by a same-file definition are skipped).
- Impact follows aliased uses too, and stored qualifiers now decide ties at
  any rank (namespace imports tie below tier 2 since tiers can't see them;
  there the qualifier must resolve through an import row).
- `dependencies`/`dependents` (CLI + MCP) accept qualified symbols
  (`crate::mcp::serve`), matching `definition` (target order: file, module,
  qualified symbol, bare symbol).
- Insights dashboard Export JSON button (downloads the `/api/insights`
  payload; client-side, still fully local).
- `keel doctor` verifies the MCP server over a stdio loopback
  (`tools/list` roundtrip, 10s timeout) and reports the tool count.
- `--json` now covers `keel doctor` (`{"checks": [...]}`) and `keel index`
  (stats object), matching the query commands.
- `--json` covers `keel status` (`{"daemon": {...}, "insights": {...}}`).
- `implementations` covers TypeScript `implements`/`extends`, and
  Python/JavaScript class bases, in addition to Rust traits (Go interfaces
  stay out: they are implicit and cannot be extracted soundly).
- `keel status` reports this project's Insights server (`running on port N`
  / `stopped`).
- `keel init` MCP snippet also prints the Claude Code registration command.
- `keel doctor` follow-up advice is daemon-state aware: missing index
  suggests `keel init` (works daemon-less) when the daemon is down, and
  project registration names `keel daemon` first — no more dead-end
  `keel start` instructions.
- MCP `dependencies`/`dependents` argument docs now name all accepted target
  kinds (module path, file path, symbol, qualified symbol), and the trust
  envelope gloss teaches tier ordering (1 = strongest evidence).
- HTTP `GET /impact/{name}[?module=M]` and `GET /dependents/{target}`:
  same payloads and trust envelopes as CLI/MCP (completes HTTP parity —
  every query tool is now reachable over HTTP).
- `dependencies`/`dependents` (CLI + MCP + HTTP) accept directories
  (`src/graph`, trailing `/` and `./` tolerated) and parent modules
  (`crate::graph` covers `crate::graph::resolve`): subtree targets resolve
  the whole tree, not just `mod.rs`.
- `outline` (CLI + MCP + HTTP) accepts module paths and directories in
  addition to files (bare symbol names stay a miss). Module nesting covers
  all separators (`::`, `.`, `/`), so Python `src.pkg` and Go paths work
  too; multi-file outlines list files in path order, symbols in source
  order.
- `implementations` (CLI + MCP) accepts `--module`/`module` and qualified
  names (`crate::a::Store`): impls attribute to the trait's module by
  co-location or import evidence, same-named traits warn when unfiltered,
  and wrong-module misses stay honest.
- `keel insights` reuse and `keel status` (text + `--json`) report a
  `UNHEALTHY`/`api_error` state when the running server answers `/health`
  but its API fails (e.g. a pre-upgrade server facing a newer index
  schema), with the exact restart remedy — no more silently broken
  dashboards after upgrades.
- `keel init` MCP snippets pin the project root (`cwd` + `KEEL_INDEX_DB`
  in the Cursor JSON, `-e KEEL_INDEX_DB=…` in the Claude Code command),
  so globally-installed servers resolve this project's index no matter
  which cwd the client spawns them with.
- `references`/`callers`/`impact` cap hit lists at 500 by default
  (override: `--limit`/`limit`/`?limit=`, 1–100000) with an explicit
  "Showing first N of M" note, so hot names stop blowing agent context
  windows. CLI text mode now prints notes on hits too (previously
  miss-only), keeping the cap visible everywhere.
- The indexer prunes vendor/build/cache dirs (`node_modules/`, `target/`,
  `dist/`, `vendor/`, `__pycache__/`, `.venv/`, …) even when no `.gitignore`
  covers them, so vendored copies stop polluting results (`build/` and `out/`
  are still indexed: they sometimes hold hand-written scripts).
- Go value-position reads are references, completing all five languages:
  `return` operands, binary/unary operands, parenthesized expressions,
  `if`/`for` conditions, `switch` subjects, `case` values, index/slice
  sides, channel sends (`ch <- v`), type-assertion operands,
  `range` iterables (targets bind), composite elements and field values
  (keys are labels), `var`/`const` initializers, `:=` right sides,
  assignment RHS (plus compound-assignment targets; plain LHS stays a
  write), `n++`/`n--` targets, and spread operands. Each site emits
  exactly once; bindings, blanks, and field sides stay excluded. Impact
  benchmark grows to 32 queries (F1 1.0).
- TypeScript/JavaScript members are definitions and member reads are
  references: class fields, property signatures, and enum members index
  as `field` symbols (computed/string/number/private names stay out),
  and `obj.prop` reads the property as a `value` reference qualified by
  the receiver text, mirroring method calls. Call/new callees, JSX
  tags, heritage clauses, and decorator-owned members stay with their
  arms; properties in write positions (plain-assignment LHS, `for-in`
  targets, `delete` operands) are writes, while updates and compound
  targets read. Chains read each level once. Module-scoped
  `references`/`callers`/`impact` attribute member reads through the
  use file's imports: when nothing resolves precisely and the target
  module is imported (under any binding — the import names the class,
  not the field), the read attributes there. Impact benchmark grows to
  34 queries (F1 1.0).
- Python members are definitions and attribute reads are references:
  class-level bindings (covering dataclass fields and enum members)
  and `self`/`cls` attribute writes index as `field` symbols, and
  `obj.attr` reads the attribute as a `value` reference qualified by
  the receiver text. Call callees, decorator applications, and
  annotation/handler-type subtrees stay with their arms; attributes in
  write positions (plain-assignment LHS, loop targets, `with`/`as`
  targets, `del` targets, dict keys — through tuple/list/paren target
  wrappers) are writes, while augmented targets read. Chains read each
  level once. Impact benchmark grows to 35 queries (F1 1.0).
- Rust members are definitions and field reads are references:
  struct/union fields and enum variants index as `field` symbols, and
  `obj.field` reads the field as a `value` reference qualified by the
  receiver text. Call callees stay with the call arm; fields in
  plain-assignment LHS position (through tuple/array/paren targets)
  are writes, while compound targets read. Chains read each level
  once; tuple indices (`a.0`) have no name and stay out. Enum
  discriminants (`A = LIMIT`) read too. Impact benchmark grows to 36
  queries (F1 1.0).
- Go members are definitions and selector reads are references,
  completing all five languages: struct fields index as `field`
  symbols (multi-name declarations each count; embedded fields carry
  a type, not a name), and `obj.Field` reads the field as a `value`
  reference qualified by the operand text. Call callees stay with the
  call arm; fields in plain-assignment/range/receive LHS position
  (through multi-target lists) are writes, while compound and
  inc/dec targets read. Chains read each level once. Impact
  benchmark grows to 37 queries (F1 1.0).
- Method calls on locals resolve through the use file's imports: like
  member reads, `c.describe()` attributes to the imported module
  defining the name when nothing resolves precisely — previously
  only same-name namespace calls did. A qualifier resolving through
  an import row to another ranked module still wins as precise
  evidence. Impact benchmark grows to 39 queries (F1 1.0).

### Fixed

- Strict qualifier matching no longer treats bare equality as evidence:
  `c.describe()` used to attribute to module `c` whenever the names
  coincided, even with unrelated imports on file. Exact matches now
  need a path-like qualifier (`crate::mcp`) or an import row
  mentioning the qualifier; absolute paths still match row-free.
- Cross-scheme last-segment imports no longer outrank exact ones: the
  Go-style fallback (`example.com/…/h` → package `h`) now resolves at
  tier 2 instead of tying tier 1 with the exact import, so a same-named
  Go package can no longer steal `impact`/`callers` attribution from a
  TypeScript (`ts/src/h`) or Rust (`crate::h`) module it merely shares a
  trailing segment with. Go dot-imports (`import . "…"`) reach bare
  names through the import tier as well.
- `keel index`/`watch`/`init` and the MCP `index` tool reject paths that
  are not existing directories (`no such directory` / `not a directory`,
  nonzero exit) instead of creating junk `<typo>/.keel/` dirs and reporting
  a zero-file "success".
- The MCP `index` tool writes the target's own `<path>/.keel/index.db`
  instead of reconciling the foreign root into the server's database
  (which wiped the server project's rows and answered with foreign paths).
- `keel serve` announces its URL only after the port bind succeeds instead
  of claiming "Serving …" before a privileged/taken port fails.
- `keel insights-stop`, `keel daemon-stop`, and daemon startup verify a
  recorded pid still belongs to Keel (Linux `/proc` cmdline) before
  trusting or signaling it: stale pidfiles/markers after PID reuse are
  cleaned up instead of killing an unrelated process or blocking startup.
- `impact` no longer drops call sites whose container holds a dotted or
  slashed module path: the file-path guard mistook Python `src.b::beta` and
  TypeScript `src/b::beta` for file paths, so impact was effectively empty
  outside Rust. Qualified (`::`) containers are now always resolved as
  symbol identities.
- `impact` resolves nested class scopes (`src.b::Beta::run`): methods are
  stored under the file module, so enclosing scopes are stripped inside-out
  until an exact module+name match hits (most specific first).
- Insights dashboard surfaces the server's JSON error body on failure
  (`HTTP 500: schema version …`) instead of a bare status code.
- `impact` no longer matches cross-file method calls with no import against an
  unrelated same-named free function (`client.get()` vs the only free `get`);
  same-file and imported method calls still count.
- `definition`/`dependencies`/`dependents`/`implementations`/`outline`
  text output prints trust-envelope notes on hits (they were silently
  dropped), so truncation, disambiguation, and file-scope notes are visible
  outside `--json`.
- Empty query input fails loudly on every surface instead of printing
  confusing miss headers: the CLI rejects `keel definition ""` (exit 2,
  `name must not be empty`), MCP returns a JSON-RPC error for empty required
  arguments (matching HTTP's 400), and `--module ""` means "no filter"
  instead of reporting `in module ```. The `search` miss header also
  backtick-quotes the pattern like the trust-envelope note does.
- Daemon, Insights-server, and managed-process failures report `daemon
  error: …` instead of the misleading `watch error: …` (new `KeelError::Daemon`
  variant); `watch error:` now only labels real filesystem-watcher failures.

## [1.4.1] — 2026-09-27

### Fixed

- Placate clippy 1.98 `collapsible_match` in the Python extractor so the
  release gate passes (no behavior change).

## [1.4.0] — 2026-09-27

### Added

- Insights portal: `keel serve` exposes `/insights` (local dashboard) and
  `/api/insights` (JSON) with index health, usage rollups, confidence mix,
  miss recovery, and recent queries. Per-project `.keel/usage.jsonl` event
  log (5 MiB rotation, `KEEL_NO_USAGE_LOG=1` opt-out, local only).
- `keel insights [--port N]` (alias `keel insight`): open the dashboard
  without running `serve` manually (starts a background server when needed,
  reuses it via `.keel/insights.port`, adopts a hand-started `keel serve`
  for the same project, picks a free port when the preferred one is busy,
  `KEEL_NO_BROWSER=1` skips the browser launch, `--json` prints the URL).
- `keel init` (index now + print paste-ready MCP config; works without the
  daemon, registers for live watching when one runs), `keel doctor`
  (daemon/project/index health), `keel daemon-stop`, and `--version`.
- Miss recovery: empty results suggest near-matches
  (`Did you mean …?`) across CLI/MCP/JSON; case-only misses recover.
- `dependencies` marks unindexed modules `external` (tag in CLI, field in
  JSON/MCP) so agents can filter stdlib noise.
- Bare-identifier references in Python/TypeScript/JavaScript: call-argument
  values, type annotations, and (Python) class bases/decorator args, with a
  new `value` reference kind.
- Index content-format stamping (`meta` table, schema v3): upgrades rebuild
  stale indexes automatically instead of serving outdated answers; reads
  against stale content without auto-index fail with an actionable error.
- Empty-index misses say the index is empty (with the fix) instead of a bare
  confident miss; empty `impact` on existing symbols says "No impacted
  symbols found."
- `keel start` / `index` keep `.keel/` out of version control automatically.

### Fixed

- `keel index <path>` / `keel watch <path>` use `<path>/.keel/index.db`
  instead of the cwd's (out-of-tree indexing no longer wipes on query).
- Dependency edges require import or same-module evidence; unimported
  name-only matches (e.g. `client.get` vs an unrelated `get`) no longer
  fabricate edges. `impact` intentionally keeps the looser candidate rule.
- `references` / `callers` human output includes the kind column, matching
  `definition` / `impact`.
- Daemon start hints are platform-aware (`keel daemon` outside macOS);
  stale `callers` help text updated.
- Invalid registry pids are never signaled (pid validation before `kill`).

## [1.3.1] — 2026-08-02

### Fixed

- Clippy `unnecessary_unwrap` / `len_zero` cleanups so the Release workflow gate
  passes (1.3.0 tag had no published binaries).

## [1.3.0] — 2026-08-02

### Added

- Optional MCP `module` argument (and qualified names like `crate::mcp::serve`)
  for `definition` / `references` / `callers` / `impact` disambiguation.
- MCP tool descriptions and Cursor rule guidance for reading `confidence` /
  `notes` before acting.
- `find_impact_from_defs` so impact seeds a specific qualified identity.
- `scripts/keel-mcp.sh` prefers workspace `target/{release,debug}/keel` over
  Homebrew so local agent fixes are visible.
- `scripts/mcp-trust-smoke.py` + CI workflow asserting the agent trust envelope.
- MCP `readOnlyHint` annotations on query tools.

### Fixed

- Empty query results no longer claim “name-only fallback”; they report a
  confident “No matching symbols found” miss (`resolution_tier: 0`).
- Multi-definition responses no longer append impact-only over-approx notes to
  non-impact queries.
- `references` with `module` / qualified name filter via import-aware resolve
  (same path as callers) instead of name-only hits with High confidence.
- `impact` no longer re-seeds from all bare-name definitions after disambiguation;
  non-empty impact is always `medium`/`low` with a candidate-blast-radius note
  (never fabricated High tiers).
- Passing both `module` and a qualified `name` strips to the bare symbol.
- Ambiguous daemon registry (multiple projects, cwd in none) no longer picks the
  most recently modified index — falls back to `cwd/.keel/index.db`.

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
