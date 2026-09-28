//! Stable public library facade for Keel consumers.
//!
//! Prefer [`Index`] over reaching into `db` / `graph` / `index` modules directly.
//! Internals remain available for the CLI and advanced use.

use crate::db::{queries, schema};
use crate::error::{KeelError, Result};
use crate::graph::deps::{self, Dependency};
use crate::graph::impact;
use crate::graph::query_result::QueryResult;
use crate::graph::resolve;
use crate::graph::target;
use crate::graph::types::{ImplRecord, Reference, Symbol};
use crate::index::{self, IndexStats};
use crate::languages::Registry;
use rusqlite::Connection;
use std::collections::HashSet;
use std::path::Path;

/// Opened Keel index (SQLite-backed).
///
/// This is the stable library entry point for indexing and querying.
pub struct Index {
    conn: Connection,
}

impl Index {
    /// Open (or create) an on-disk index database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path.as_ref())?;
        crate::db::configure_connection(&conn)?;
        schema::initialize(&conn)?;
        Ok(Self { conn })
    }

    /// Open an in-memory index (useful for tests and ephemeral analysis).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        crate::db::configure_connection(&conn)?;
        schema::initialize(&conn)?;
        Ok(Self { conn })
    }

    /// Index every registered-language source file under `root`.
    pub fn index_path(&mut self, root: &Path) -> Result<IndexStats> {
        index::index_repository(root, &mut self.conn)
    }

    /// Index `root` using a custom [`Registry`] (community language plugins).
    pub fn index_path_with(
        &mut self,
        root: &Path,
        registry: &Registry,
    ) -> Result<IndexStats> {
        index::index_repository_with(root, &mut self.conn, registry)
    }

    /// Find definitions matching `name`.
    pub fn definition(&self, name: &str) -> Result<Vec<Symbol>> {
        Ok(self.definition_with_meta(name)?.results)
    }

    /// Definitions plus confidence metadata.
    pub fn definition_with_meta(&self, name: &str) -> Result<QueryResult<Symbol>> {
        definition_with_meta(&self.conn, name)
    }

    /// Definitions with optional module disambiguation.
    pub fn definition_with_meta_opts(
        &self,
        name: &str,
        module: Option<&str>,
    ) -> Result<QueryResult<Symbol>> {
        definition_with_meta_opts(&self.conn, name, module)
    }

    /// Find references matching `name`.
    pub fn references(&self, name: &str) -> Result<Vec<Reference>> {
        Ok(self.references_with_meta(name)?.results)
    }

    /// References plus confidence metadata.
    pub fn references_with_meta(&self, name: &str) -> Result<QueryResult<Reference>> {
        references_with_meta(&self.conn, name)
    }

    /// References with optional module disambiguation for the defining symbol.
    pub fn references_with_meta_opts(
        &self,
        name: &str,
        module: Option<&str>,
    ) -> Result<QueryResult<Reference>> {
        references_with_meta_opts(&self.conn, name, module)
    }

    /// Find callers of `name` (import-aware when a unique definition module exists).
    pub fn callers(&self, name: &str) -> Result<Vec<Reference>> {
        Ok(self.callers_with_meta(name)?.results)
    }

    /// Callers plus confidence metadata.
    pub fn callers_with_meta(&self, name: &str) -> Result<QueryResult<Reference>> {
        callers_with_meta(&self.conn, name)
    }

    /// Callers with optional module disambiguation.
    pub fn callers_with_meta_opts(
        &self,
        name: &str,
        module: Option<&str>,
    ) -> Result<QueryResult<Reference>> {
        callers_with_meta_opts(&self.conn, name, module)
    }

    /// Find trait implementations for `trait_name`.
    pub fn implementations(&self, trait_name: &str) -> Result<Vec<ImplRecord>> {
        Ok(self.implementations_with_meta(trait_name)?.results)
    }

    /// Implementations plus confidence metadata.
    pub fn implementations_with_meta(
        &self,
        trait_name: &str,
    ) -> Result<QueryResult<ImplRecord>> {
        implementations_with_meta(&self.conn, trait_name)
    }

    /// Implementations plus confidence metadata, narrowed to `module`.
    pub fn implementations_with_meta_opts(
        &self,
        trait_name: &str,
        module: Option<&str>,
    ) -> Result<QueryResult<ImplRecord>> {
        implementations_with_meta_opts(&self.conn, trait_name, module)
    }

    /// Find modules/files that `name` (module path or symbol) depends on.
    pub fn dependencies(&self, name: &str) -> Result<Vec<Dependency>> {
        Ok(self.dependencies_with_meta(name)?.results)
    }

    /// Dependencies plus confidence metadata.
    pub fn dependencies_with_meta(&self, name: &str) -> Result<QueryResult<Dependency>> {
        dependencies_with_meta(&self.conn, name)
    }

    /// Find modules that depend on `name` (module path, file, or symbol).
    pub fn dependents(&self, name: &str) -> Result<Vec<Dependency>> {
        Ok(self.dependents_with_meta(name)?.results)
    }

    /// Dependents plus confidence metadata.
    pub fn dependents_with_meta(&self, name: &str) -> Result<QueryResult<Dependency>> {
        dependents_with_meta(&self.conn, name)
    }

    /// Find symbols transitively impacted by changing `name`.
    pub fn impact(&self, name: &str) -> Result<Vec<Symbol>> {
        Ok(self.impact_with_meta(name)?.results)
    }

    /// Impact plus confidence metadata.
    pub fn impact_with_meta(&self, name: &str) -> Result<QueryResult<Symbol>> {
        impact_with_meta(&self.conn, name)
    }

    /// Impact with optional module disambiguation.
    pub fn impact_with_meta_opts(
        &self,
        name: &str,
        module: Option<&str>,
    ) -> Result<QueryResult<Symbol>> {
        impact_with_meta_opts(&self.conn, name, module)
    }

    /// Outline: symbols defined in `file`, in source order.
    pub fn outline(&self, file: &str) -> Result<Vec<Symbol>> {
        Ok(self.outline_with_meta(file)?.results)
    }

    /// Outline plus confidence metadata.
    pub fn outline_with_meta(&self, file: &str) -> Result<QueryResult<Symbol>> {
        outline_with_meta(&self.conn, file)
    }

    /// Unreferenced functions: candidate dead code in `target`
    /// (file, module, or directory; `None` = whole project).
    pub fn unused(&self, target: Option<&str>) -> Result<Vec<Symbol>> {
        Ok(self.unused_with_meta(target)?.results)
    }

    /// Unused sweep plus confidence metadata.
    pub fn unused_with_meta(
        &self,
        target: Option<&str>,
    ) -> Result<QueryResult<Symbol>> {
        unused_with_meta(&self.conn, target)
    }

    /// Unused sweep plus confidence metadata, with an optional
    /// transitive pass over references from candidate-dead functions.
    pub fn unused_with_meta_opts(
        &self,
        target: Option<&str>,
        transitive: bool,
    ) -> Result<QueryResult<Symbol>> {
        unused_with_meta_opts(&self.conn, target, transitive)
    }

    /// Substring symbol search: definitions whose name contains `pattern`.
    pub fn search(&self, pattern: &str, limit: usize) -> Result<Vec<Symbol>> {
        Ok(self.search_with_meta(pattern, limit)?.results)
    }

    /// Substring symbol search plus confidence metadata.
    pub fn search_with_meta(
        &self,
        pattern: &str,
        limit: usize,
    ) -> Result<QueryResult<Symbol>> {
        search_with_meta(&self.conn, pattern, limit)
    }
}

/// Split `module::…::symbol` into `(module_path, symbol)` when unambiguous.
///
/// Returns `None` when there is no `::` separator (bare symbol).
pub fn split_qualified_name(name: &str) -> Option<(&str, &str)> {
    let (module, symbol) = name.rsplit_once("::")?;
    if module.is_empty() || symbol.is_empty() || symbol.contains("::") {
        return None;
    }
    Some((module, symbol))
}

/// Resolve optional `module` arg or a qualified `name` into `(module, bare_name)`.
///
/// When both `module` and a qualified `name` are provided, the last `::` segment
/// is the symbol and `module` wins as the module path (agents often pass both).
fn resolve_symbol_target<'a>(
    name: &'a str,
    module: Option<&'a str>,
) -> (Option<&'a str>, &'a str) {
    // An explicitly empty module (`--module ""`) is "no filter", not a
    // module named ``: without this every query reports
    // "No definition for `x` in module ``.".
    if let Some(m) = module.filter(|m| !m.is_empty()) {
        let bare = split_qualified_name(name).map(|(_, sym)| sym).unwrap_or(name);
        return (Some(m), bare);
    }
    if let Some((m, sym)) = split_qualified_name(name) {
        return (Some(m), sym);
    }
    (None, name)
}

/// Refuse reads against content an older Keel wrote.
///
/// Normal paths (CLI/MCP/HTTP with auto-index) rebuild stale indexes before
/// reading, so this only fires for explicit `--no-auto-index` bypasses.
pub(crate) fn ensure_index_current(conn: &Connection) -> Result<()> {
    let found = schema::index_format_version(conn);
    if found != schema::INDEX_FORMAT_VERSION {
        return Err(KeelError::StaleIndex {
            found,
            current: schema::INDEX_FORMAT_VERSION,
        });
    }
    Ok(())
}

/// Flag queries against an index holding nothing: a confident miss there means
/// "not indexed", not "doesn't exist". Lookup failures are swallowed.
fn push_empty_index_note(conn: &Connection, notes: &mut Vec<String>) {
    if let Ok(true) = queries::is_index_empty(conn) {
        notes.push(
            "Index is empty (no files indexed). Run `keel index` or `keel start` in the project, then retry."
                .to_string(),
        );
    }
}

/// Notes for a bare confident miss: the canonical marker plus miss recovery.
fn miss_notes(conn: &Connection, bare: &str) -> Vec<String> {
    let mut notes = vec!["No matching symbols found.".to_string()];
    push_empty_index_note(conn, &mut notes);
    push_suggestion_note(conn, bare, &mut notes);
    notes
}

/// Append "Did you mean …?" when near-matches exist. Lookup failures are
/// swallowed: a miss stays a miss even when suggestions can't load.
fn push_suggestion_note(conn: &Connection, bare: &str, notes: &mut Vec<String>) {
    if let Ok(suggestions) = resolve::suggest_names(conn, bare, 3) {
        if !suggestions.is_empty() {
            let quoted: Vec<String> = suggestions.iter().map(|s| format!("`{s}`")).collect();
            notes.push(format!("Did you mean {}?", quoted.join(", ")));
        }
    }
}

/// Definitions plus confidence metadata (shared by [`Index`] and MCP/CLI).
pub fn definition_with_meta(conn: &Connection, name: &str) -> Result<QueryResult<Symbol>> {
    definition_with_meta_opts(conn, name, None)
}

/// Definitions with optional module filter (or qualified `name`).
pub fn definition_with_meta_opts(
    conn: &Connection,
    name: &str,
    module: Option<&str>,
) -> Result<QueryResult<Symbol>> {
    ensure_index_current(conn)?;
    let (mod_path, bare) = resolve_symbol_target(name, module);
    let results = if let Some(m) = mod_path {
        queries::find_definition_by_qualified(conn, m, bare)?
    } else {
        queries::find_definition(conn, bare)?
    };
    let multi = results.len() > 1;
    let tiers: Vec<u8> = if results.is_empty() {
        vec![]
    } else if multi {
        vec![3; results.len()]
    } else {
        vec![2]
    };
    let mut notes = Vec::new();
    if multi {
        notes.push(format!(
            "Found {} definitions for `{bare}`; disambiguate with module arg or qualified name (e.g. `crate::mcp::{bare}`).",
            results.len()
        ));
    } else if results.is_empty() {
        if let Some(m) = mod_path {
            notes.push(format!("No definition for `{bare}` in module `{m}`."));
            push_empty_index_note(conn, &mut notes);
            push_suggestion_note(conn, bare, &mut notes);
        } else {
            notes.extend(miss_notes(conn, bare));
        }
    }
    Ok(QueryResult::from_tiers(results, &tiers, multi, notes))
}

/// References plus confidence metadata.
pub fn references_with_meta(conn: &Connection, name: &str) -> Result<QueryResult<Reference>> {
    references_with_meta_opts(conn, name, None)
}

/// References with optional module disambiguation for the defining symbol.
pub fn references_with_meta_opts(
    conn: &Connection,
    name: &str,
    module: Option<&str>,
) -> Result<QueryResult<Reference>> {
    ensure_index_current(conn)?;
    let (mod_path, bare) = resolve_symbol_target(name, module);
    let defs = if let Some(m) = mod_path {
        queries::find_definition_by_qualified(conn, m, bare)?
    } else {
        queries::find_definition(conn, bare)?
    };
    let multi = defs.len() > 1;
    let target_module = mod_path
        .map(str::to_owned)
        .or_else(|| unique_module(&defs));
    // When the definition identity is known, filter refs the same way callers do
    // (module/import-aware). Never report High on unfiltered name matches after
    // a module was requested.
    let name_matches = resolve::find_callers(conn, bare, target_module.as_deref())?;
    let had_name_matches = !name_matches.is_empty();
    let (base_tier, mut notes) = if target_module.is_some() && !multi {
        (1, Vec::new())
    } else if multi {
        (
            3,
            vec![
                "Multiple definitions share this name; references fall back to name matching. Pass module or a qualified name to narrow."
                    .into(),
            ],
        )
    } else {
        (2, Vec::new())
    };
    let (results, tiers, alias_note) = merge_alias_uses(
        conn,
        bare,
        target_module.as_deref(),
        name_matches,
        base_tier,
    )?;
    if let Some(note) = alias_note {
        // Alias-only hits aren't name matches: drop the stale fallback note.
        if multi && !had_name_matches {
            notes.clear();
        }
        notes.push(note);
    }
    if results.is_empty() && defs.is_empty() {
        if let Some(m) = mod_path {
            notes.push(format!("No definition for `{bare}` in module `{m}`."));
            push_empty_index_note(conn, &mut notes);
            push_suggestion_note(conn, bare, &mut notes);
        } else {
            notes.extend(miss_notes(conn, bare));
        }
    }
    if results.is_empty() && !defs.is_empty() {
        // Definitions matched, so the canonical "no matching symbols" marker
        // would lie: name the empty site list honestly instead.
        if let Some(m) = mod_path {
            let total = queries::find_references(conn, bare)?.len();
            if total > 0 {
                let sites = if total == 1 { "site" } else { "sites" };
                notes.push(format!(
                    "{total} {sites} name-match `{bare}` but none resolved to module `{m}`: qualified call sites can't be attributed while several modules define the name."
                ));
            } else {
                notes.push(format!("No reference sites recorded for `{bare}`."));
            }
        } else {
            notes.push(format!("No reference sites recorded for `{bare}`."));
        }
    }
    Ok(QueryResult::from_tiers(results, &tiers, multi, notes))
}

/// Callers plus confidence metadata.
pub fn callers_with_meta(conn: &Connection, name: &str) -> Result<QueryResult<Reference>> {
    callers_with_meta_opts(conn, name, None)
}

/// Callers with optional module disambiguation.
pub fn callers_with_meta_opts(
    conn: &Connection,
    name: &str,
    module: Option<&str>,
) -> Result<QueryResult<Reference>> {
    ensure_index_current(conn)?;
    let (mod_path, bare) = resolve_symbol_target(name, module);
    let defs = if let Some(m) = mod_path {
        queries::find_definition_by_qualified(conn, m, bare)?
    } else {
        queries::find_definition(conn, bare)?
    };
    let multi = defs.len() > 1;
    let target_module = mod_path
        .map(str::to_owned)
        .or_else(|| unique_module(&defs));
    let name_matches = resolve::find_callers(conn, bare, target_module.as_deref())?;
    let had_name_matches = !name_matches.is_empty();
    let (base_tier, mut notes) = if target_module.is_some() && !multi {
        (1, Vec::new())
    } else {
        let mut n = Vec::new();
        if multi {
            n.push(
                "No unique definition module; callers fall back to name matching. Pass module or a qualified name."
                    .into(),
            );
        }
        (3, n)
    };
    let (results, tiers, alias_note) = merge_alias_uses(
        conn,
        bare,
        target_module.as_deref(),
        name_matches,
        base_tier,
    )?;
    if let Some(note) = alias_note {
        // Alias-only hits aren't name matches: drop the stale fallback note.
        if multi && !had_name_matches {
            notes.clear();
        }
        notes.push(note);
    }
    if results.is_empty() && defs.is_empty() {
        if let Some(m) = mod_path {
            notes.push(format!("No definition for `{bare}` in module `{m}`."));
            push_empty_index_note(conn, &mut notes);
            push_suggestion_note(conn, bare, &mut notes);
        } else {
            notes.extend(miss_notes(conn, bare));
        }
    }
    if results.is_empty() && !defs.is_empty() {
        // Definitions matched, so the canonical "no matching symbols" marker
        // would lie: name the empty caller list honestly instead.
        if let Some(m) = mod_path {
            let total = queries::find_references(conn, bare)?.len();
            if total > 0 {
                let sites = if total == 1 { "site" } else { "sites" };
                notes.push(format!(
                    "{total} {sites} name-match `{bare}` but none resolved to module `{m}`: qualified call sites can't be attributed while several modules define the name."
                ));
            } else {
                notes.push(format!("No call sites recorded for `{bare}`."));
            }
        } else {
            notes.push(format!("No call sites recorded for `{bare}`."));
        }
    }
    Ok(QueryResult::from_tiers(results, &tiers, multi, notes))
}

/// Implementations plus confidence metadata.
pub fn implementations_with_meta(
    conn: &Connection,
    trait_name: &str,
) -> Result<QueryResult<ImplRecord>> {
    implementations_with_meta_opts(conn, trait_name, None)
}

/// Implementations plus confidence metadata, narrowed to `module` (or a
/// qualified `crate::m::Trait` name) when given.
///
/// Impl records carry no trait module, so attribution is by evidence:
/// an impl counts for module `M` when its file defines `M` (co-located)
/// or imports `M` (by module or by `M::Trait` symbol path).
pub fn implementations_with_meta_opts(
    conn: &Connection,
    name: &str,
    module: Option<&str>,
) -> Result<QueryResult<ImplRecord>> {
    ensure_index_current(conn)?;
    let (mod_path, bare) = resolve_symbol_target(name, module);
    let mut results = queries::find_implementations(conn, bare)?;
    let mut notes = Vec::new();
    let mut multi = false;
    if let Some(m) = mod_path {
        let defs = queries::find_definition_by_qualified(conn, m, bare)?;
        if defs.is_empty() {
            notes.push(format!("No definition for `{bare}` in module `{m}`."));
            push_empty_index_note(conn, &mut notes);
            push_suggestion_note(conn, bare, &mut notes);
            return Ok(QueryResult::from_tiers(Vec::new(), &[], false, notes));
        }
        let before = results.len();
        results.retain(|r| impl_attributed_to_module(conn, r, m, bare));
        if results.len() != before {
            notes.push(format!(
                "Filtered to module `{m}` ({} of {before} implementation(s)).",
                results.len()
            ));
        }
    } else if !results.is_empty() {
        let defs = queries::find_definition(conn, bare)?;
        multi = defs.len() > 1;
        let mut modules: Vec<&str> =
            defs.iter().map(|d| d.module_path.as_str()).collect();
        modules.sort_unstable();
        modules.dedup();
        if modules.len() > 1 {
            notes.push(
                "Multiple traits share this name; pass module or a qualified name to narrow."
                    .into(),
            );
        }
    }
    if results.is_empty() && notes.is_empty() {
        notes.push(
            "No implementations found (covers Rust traits, TypeScript interfaces, Python/JavaScript base classes, and explicit Go assertions; structural Go matches are not inferred)."
                .into(),
        );
        push_empty_index_note(conn, &mut notes);
    }
    let tiers = if results.is_empty() {
        vec![]
    } else {
        vec![2; results.len()]
    };
    Ok(QueryResult::from_tiers(results, &tiers, multi, notes))
}

/// Attribute an impl record to trait module `m`: same file defines `M`,
/// or the impl's file imports `M` (bare module or `M::trait` path).
fn impl_attributed_to_module(
    conn: &Connection,
    record: &crate::graph::types::ImplRecord,
    module: &str,
    bare: &str,
) -> bool {
    let file = record.file.to_string_lossy();
    if let Ok(modules) = queries::module_paths_in_file(conn, file.as_ref()) {
        if modules.iter().any(|m| m == module) {
            return true;
        }
    }
    if let Ok(imports) = queries::imports_for_file(conn, file.as_ref()) {
        if imports
            .iter()
            .any(|(import, _)| import_selects_module(import, module, bare))
        {
            return true;
        }
    }
    false
}

/// Whether an import row plausibly selects trait module `module`: the exact
/// module, the exact `module<sep>trait` symbol path, or a relative spelling
/// (`a::Store`, `a`) that the module path ends with. Separators cover all
/// languages (`::`, `.`, `/`).
fn import_selects_module(import: &str, module: &str, bare: &str) -> bool {
    if import == module {
        return true;
    }
    for sep in ["::", ".", "/"] {
        if import == format!("{module}{sep}{bare}") {
            return true;
        }
        if let Some(prefix) = import.strip_suffix(&format!("{sep}{bare}")) {
            if prefix == module || module.ends_with(&format!("{sep}{prefix}")) {
                return true;
            }
        }
    }
    // Bare relative module (`use a` for `crate::a`).
    module == import
        || ["/", "::", "."]
            .iter()
            .any(|sep| module.ends_with(&format!("{sep}{import}")))
}

/// Outline plus confidence metadata: exact file lookup, High on hits.
pub fn outline_with_meta(conn: &Connection, raw: &str) -> Result<QueryResult<Symbol>> {
    ensure_index_current(conn)?;
    // Files first (with `./`/absolute tolerance), then module subtrees and
    // directories; bare symbol names stay a miss with suggestions.
    let files: Vec<String> = if let Some(file) = resolve_outline_file(conn, raw)? {
        vec![file]
    } else if let Some(more) = target::module_or_dir_files(conn, raw)? {
        more
    } else {
        let mut notes = vec![format!(
            "No indexed file, module, or directory matching `{raw}`."
        )];
        push_file_suggestion_note(conn, raw, &mut notes);
        push_empty_index_note(conn, &mut notes);
        return Ok(QueryResult::from_tiers(Vec::new(), &[], false, notes));
    };
    let mut results = Vec::new();
    for file in &files {
        results.extend(queries::symbols_in_file(conn, file)?);
    }
    if results.is_empty() {
        let notes = if files.len() == 1 {
            vec![format!(
                "File `{}` is indexed but declares no symbols.",
                files[0]
            )]
        } else {
            vec![format!(
                "`{raw}` covers {} indexed file(s) but declares no symbols.",
                files.len()
            )]
        };
        return Ok(QueryResult::from_tiers(results, &[], false, notes));
    }
    let tiers = vec![1; results.len()];
    Ok(QueryResult::from_tiers(results, &tiers, false, Vec::new()))
}

/// Notes carried by every non-empty `unused` sweep: candidates, not a
/// delete list.
fn unused_candidate_notes() -> Vec<String> {
    vec![
        "Candidates only: dynamic dispatch, reflection, framework entry points, test attributes, and public-API/cross-repo uses are invisible — verify before deleting."
            .into(),
    ]
}

/// True when `symbol` is an entry point or test/framework hook that an
/// `unused` sweep must never report, regardless of references.
fn is_unused_exempt(symbol: &Symbol) -> bool {
    if symbol.name == "main" || symbol.name == "constructor" {
        return true;
    }
    // Inline test modules (`mod tests` in Rust, `tests.*` packages): the
    // harness calls these by attribute/convention, never by name.
    if symbol
        .module_path
        .split([':', '.'])
        .any(|segment| matches!(segment, "test" | "tests"))
    {
        return true;
    }
    let path = symbol.file.to_string_lossy();
    if is_test_path(&path) {
        return true;
    }
    // Go implicit entries: package initializers plus the testing
    // harness conventions (`go test`/`go run` call these by name shape,
    // never through a recorded reference).
    if path.ends_with(".go")
        && (symbol.name == "init"
            || symbol.name.starts_with("Test")
            || symbol.name.starts_with("Benchmark")
            || symbol.name.starts_with("Example")
            || symbol.name.starts_with("Fuzz"))
    {
        return true;
    }
    if path.ends_with(".py") && is_python_dunder(&symbol.name) {
        return true;
    }
    // React class components: the reconciler calls `render`, user code
    // never does. (Other framework lifecycles stay candidates; the
    // sweep note says to verify.)
    if symbol.name == "render"
        && (path.ends_with(".js")
            || path.ends_with(".jsx")
            || path.ends_with(".ts")
            || path.ends_with(".tsx"))
    {
        return true;
    }
    false
}

/// True for `__name__`-style dunder hooks (`__init__`, `__main__`, …).
fn is_python_dunder(name: &str) -> bool {
    name.len() > 4 && name.starts_with("__") && name.ends_with("__")
}

/// True when `path` lives under a test directory or is a test file by
/// naming convention (case-insensitive).
fn is_test_path(path: &str) -> bool {
    let lowered = path.to_lowercase();
    let file = lowered.rsplit(['/', '\\']).next().unwrap_or(&lowered);
    if file.starts_with("test_")
        || file.starts_with("test-")
        || file.ends_with("_test.go")
        || file.ends_with(".test.ts")
        || file.ends_with(".test.js")
        || file.ends_with(".test.tsx")
        || file.ends_with(".test.jsx")
        || file.ends_with(".spec.ts")
        || file.ends_with(".spec.js")
    {
        return true;
    }
    lowered.split(['/', '\\']).any(|segment| {
        matches!(
            segment,
            "test" | "tests" | "__tests__" | "spec" | "testing" | "testdata"
        )
    })
}

/// Unreferenced-function sweep plus confidence metadata.
///
/// `target` scopes the sweep to a file, module subtree, or directory
/// (same resolution as [`outline_with_meta`]); `None` sweeps the whole
/// project. Only `function` symbols with zero same-named references are
/// reported, minus entry-point exemptions. Non-empty results are Low
/// confidence candidates; an empty sweep reports High with a caveat note.
pub fn unused_with_meta(
    conn: &Connection,
    target: Option<&str>,
) -> Result<QueryResult<Symbol>> {
    unused_with_meta_opts(conn, target, false)
}

/// Unused sweep with an optional transitive pass: when `transitive`,
/// functions referenced only from other candidate-dead functions are
/// included too (reference containers that match a flagged
/// `{module}::{function}` do not keep a name alive).
pub fn unused_with_meta_opts(
    conn: &Connection,
    target: Option<&str>,
    transitive: bool,
) -> Result<QueryResult<Symbol>> {
    ensure_index_current(conn)?;
    let files: Option<Vec<String>> = match target {
        None => None,
        Some(raw) => {
            if let Some(file) = resolve_outline_file(conn, raw)? {
                Some(vec![file])
            } else if let Some(more) = target::module_or_dir_files(conn, raw)? {
                Some(more)
            } else {
                let mut notes = vec![format!(
                    "No indexed file, module, or directory matching `{raw}`."
                )];
                push_file_suggestion_note(conn, raw, &mut notes);
                push_empty_index_note(conn, &mut notes);
                return Ok(QueryResult::from_tiers(Vec::new(), &[], false, notes));
            }
        }
    };
    let mut results = if transitive {
        unused_transitive(conn, files.as_deref())?
    } else {
        queries::unreferenced_functions(conn, files.as_deref())?
    };
    results.retain(|s| !is_unused_exempt(s));
    if results.is_empty() {
        let mut notes = vec![
            "Sweep covers recorded references only; dynamic uses are invisible."
                .to_string(),
        ];
        push_empty_index_note(conn, &mut notes);
        return Ok(QueryResult::from_tiers(results, &[], false, notes));
    }
    let tiers = vec![3; results.len()];
    let mut notes = unused_candidate_notes();
    if transitive {
        notes.push(
            "Transitive pass: includes functions referenced only from other candidates."
                .to_string(),
        );
    }
    Ok(QueryResult::from_tiers(results, &tiers, false, notes))
}

/// Fixpoint dead-code pass: a function is dead when no reference from
/// outside the dead set names it. Containers identify the enclosing
/// function exactly (`{module}::{function}`), so only references truly
/// inside flagged functions are discounted. Monotonic, hence terminating.
/// Liveness is always project-global; `files` only scopes reporting.
/// References are indexed by name so large repos stay interactive.
fn unused_transitive(
    conn: &Connection,
    files: Option<&[String]>,
) -> Result<Vec<Symbol>> {
    use std::collections::HashMap;
    let candidates: Vec<Symbol> = queries::all_functions(conn, files)?
        .into_iter()
        .filter(|s| !is_unused_exempt(s))
        .collect();
    let refs = queries::all_reference_names_with_containers(conn)?;
    let mut by_name: HashMap<&str, Vec<&str>> = HashMap::new();
    for (name, container) in &refs {
        by_name
            .entry(name.as_str())
            .or_default()
            .push(container.as_str());
    }
    // `{module}::{function}` keys already proven dead this pass.
    let mut dead: HashSet<String> = HashSet::new();
    let mut pending: Vec<usize> = (0..candidates.len()).collect();
    loop {
        let round_len = pending.len();
        let mut still_pending = Vec::with_capacity(round_len);
        for index in std::mem::take(&mut pending) {
            let symbol = &candidates[index];
            let live = by_name
                .get(symbol.name.as_str())
                .is_some_and(|containers| {
                    containers.iter().any(|container| {
                        // File-scope references (empty container) always count.
                        container.is_empty() || !dead.contains(*container)
                    })
                });
            if live {
                still_pending.push(index);
            } else {
                dead.insert(format!("{}::{}", symbol.module_path, symbol.name));
            }
        }
        if still_pending.len() == round_len {
            break;
        }
        pending = still_pending;
    }
    Ok(candidates
        .into_iter()
        .filter(|s| dead.contains(&format!("{}::{}", s.module_path, s.name)))
        .collect())
}

/// Substring symbol search plus confidence metadata.
///
/// Ranks exact (case-sensitive) matches first, then case-insensitive prefix
/// matches, then remaining substring hits; location order breaks ties.
/// `limit` is clamped to 1..=200 by the query layer.
pub fn search_with_meta(
    conn: &Connection,
    pattern: &str,
    limit: usize,
) -> Result<QueryResult<Symbol>> {
    ensure_index_current(conn)?;
    if pattern.is_empty() {
        return Ok(QueryResult::from_tiers(
            Vec::new(),
            &[],
            false,
            vec!["Empty search pattern.".into()],
        ));
    }
    let (mut results, truncated) = queries::search_symbols(conn, pattern, limit)?;
    if results.is_empty() {
        let mut notes = vec![format!("No symbols matching `{pattern}`.")];
        push_suggestion_note(conn, pattern, &mut notes);
        push_empty_index_note(conn, &mut notes);
        return Ok(QueryResult::from_tiers(results, &[], false, notes));
    }
    let lowered = pattern.to_lowercase();
    results.sort_by_key(|s| {
        if s.name == pattern {
            0u8
        } else if s.name.to_lowercase().starts_with(&lowered) {
            1u8
        } else {
            2u8
        }
    });
    let tiers: Vec<u8> = results
        .iter()
        .map(|s| {
            if s.name == pattern || s.name.to_lowercase().starts_with(&lowered) {
                1
            } else {
                2
            }
        })
        .collect();
    let mut notes = Vec::new();
    if !results.iter().any(|s| s.name == pattern) {
        notes.push(format!(
            "No exact match for `{pattern}`; showing partial matches."
        ));
    }
    if truncated {
        notes.push(format!(
            "Showing first {} matches; narrow the pattern for more.",
            results.len()
        ));
    }
    Ok(QueryResult::from_tiers(results, &tiers, false, notes))
}

/// Resolve a user-supplied file path to an indexed path.
///
/// Tries the raw string, then a lexically cleaned form (`./x`, `a/../b`),
/// then a unique suffix match (absolute paths into the indexed tree).
fn resolve_outline_file(conn: &Connection, raw: &str) -> Result<Option<String>> {
    let files = queries::indexed_files(conn)?;
    if files.iter().any(|f| f == raw) {
        return Ok(Some(raw.to_string()));
    }
    let cleaned = clean_path(raw);
    if cleaned != raw && files.iter().any(|f| f == &cleaned) {
        return Ok(Some(cleaned));
    }
    let norm_raw = raw.replace('\\', "/");
    let mut hits = files.iter().filter(|f| {
        let norm_file = f.replace('\\', "/");
        norm_raw.len() > norm_file.len()
            && norm_raw.ends_with(norm_file.as_str())
            && norm_raw.as_bytes()[norm_raw.len() - norm_file.len() - 1] == b'/'
    });
    match (hits.next(), hits.next()) {
        (Some(only), None) => Ok(Some(only.clone())),
        _ => Ok(None),
    }
}

/// Lexically normalize a path (`./`, `a/../b`) without touching the fs.
///
/// Bails out (returns `raw`) when `..` would escape past the start.
fn clean_path(raw: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let absolute = raw.starts_with('/');
    let normalized = raw.replace('\\', "/");
    for comp in normalized.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return raw.to_string();
                }
            }
            c => parts.push(c),
        }
    }
    let mut out = parts.join("/");
    if absolute {
        out.insert(0, '/');
    }
    out
}

/// "Did you mean …?" over indexed file paths (miss path only).
fn push_file_suggestion_note(conn: &Connection, raw: &str, notes: &mut Vec<String>) {
    let Ok(files) = queries::indexed_files(conn) else {
        return;
    };
    let suggestions = resolve::rank_candidates(raw, &files, 3);
    if !suggestions.is_empty() {
        let quoted: Vec<String> = suggestions.iter().map(|s| format!("`{s}`")).collect();
        notes.push(format!("Did you mean {}?", quoted.join(", ")));
    }
}

/// Merge aliased uses (`serveA()` for `a::serve`) into name-matched results.
///
/// Alias hits carry exact module evidence, so they always rank tier 1; the
/// merged list stays ordered by location. Returns the combined results, their
/// tiers, and a note naming the aliases (none when there are no alias hits).
fn merge_alias_uses(
    conn: &Connection,
    bare: &str,
    target_module: Option<&str>,
    name_matches: Vec<Reference>,
    base_tier: u8,
) -> Result<(Vec<Reference>, Vec<u8>, Option<String>)> {
    let alias_uses = resolve::find_alias_uses(conn, bare, target_module)?;
    if alias_uses.is_empty() {
        let tiers = vec![base_tier; name_matches.len()];
        return Ok((name_matches, tiers, None));
    }
    let mut aliases: Vec<String> = alias_uses.iter().map(|r| format!("`{}`", r.name)).collect();
    aliases.sort();
    aliases.dedup();
    let hits = alias_uses.len();
    let mut pairs: Vec<(Reference, u8)> =
        name_matches.into_iter().map(|r| (r, base_tier)).collect();
    pairs.extend(alias_uses.into_iter().map(|r| (r, 1)));
    pairs.sort_by(|a, b| {
        (a.0.file.clone(), a.0.start_line, a.0.start_col)
            .cmp(&(b.0.file.clone(), b.0.start_line, b.0.start_col))
    });
    let (results, tiers): (Vec<Reference>, Vec<u8>) = pairs.into_iter().unzip();
    let noun = if hits == 1 { "use" } else { "uses" };
    let note = format!(
        "Includes {hits} aliased {noun} ({} for `{bare}`).",
        aliases.join(", ")
    );
    Ok((results, tiers, Some(note)))
}

/// Dependencies plus confidence metadata.
pub fn dependencies_with_meta(
    conn: &Connection,
    name: &str,
) -> Result<QueryResult<Dependency>> {
    ensure_index_current(conn)?;
    let resolved = target::normalize_target(conn, name)?;
    let results = deps::find_dependencies(conn, name)?;
    let mut notes = Vec::new();
    if resolved.files.is_empty() {
        notes.push(format!("No indexed files found for target `{name}`."));
        push_empty_index_note(conn, &mut notes);
        return Ok(QueryResult::from_tiers(results, &[], false, notes));
    }
    if results.is_empty() {
        notes.push(format!(
            "Target resolved to {} file(s) but no import/cross-file dependencies were recorded.",
            resolved.files.len()
        ));
        return Ok(QueryResult::from_tiers(results, &[], false, notes));
    }
    Ok(QueryResult::from_tiers(results, &[1], false, notes))
}

/// Dependents (reverse dependencies) plus confidence metadata.
pub fn dependents_with_meta(
    conn: &Connection,
    name: &str,
) -> Result<QueryResult<Dependency>> {
    ensure_index_current(conn)?;
    let resolved = target::normalize_target(conn, name)?;
    let results = deps::find_dependents(conn, name)?;
    let mut notes = Vec::new();
    if resolved.files.is_empty() {
        notes.push(format!("No indexed files found for target `{name}`."));
        push_empty_index_note(conn, &mut notes);
        return Ok(QueryResult::from_tiers(results, &[], false, notes));
    }
    if results.is_empty() {
        notes.push(format!(
            "Target resolved to {} file(s) but no indexed module imports it or references its symbols.",
            resolved.files.len()
        ));
        return Ok(QueryResult::from_tiers(results, &[], false, notes));
    }
    Ok(QueryResult::from_tiers(results, &[1], false, notes))
}

/// Impact plus confidence metadata.
pub fn impact_with_meta(conn: &Connection, name: &str) -> Result<QueryResult<Symbol>> {
    impact_with_meta_opts(conn, name, None)
}

/// Impact with optional module disambiguation.
pub fn impact_with_meta_opts(
    conn: &Connection,
    name: &str,
    module: Option<&str>,
) -> Result<QueryResult<Symbol>> {
    use crate::graph::query_result::{Confidence, ResolutionTier};
    ensure_index_current(conn)?;

    let (mod_path, bare) = resolve_symbol_target(name, module);
    let defs = if let Some(m) = mod_path {
        queries::find_definition_by_qualified(conn, m, bare)?
    } else {
        queries::find_definition(conn, bare)?
    };
    let multi = defs.len() > 1;
    let mut notes = Vec::new();

    if defs.is_empty() {
        if let Some(m) = mod_path {
            notes.push(format!("No definition for `{bare}` in module `{m}`."));
            push_empty_index_note(conn, &mut notes);
            push_suggestion_note(conn, bare, &mut notes);
        } else {
            notes.extend(miss_notes(conn, bare));
        }
        return Ok(QueryResult::from_tiers(Vec::new(), &[], false, notes));
    }

    let (results, file_hits) = impact::find_impact_from_defs_with_file_hits(conn, &defs)?;

    if multi {
        notes.push(
            "Multiple definitions; impact expands each qualified identity and may over-approximate. Pass module or a qualified name to narrow."
                .into(),
        );
    }
    if !file_hits.is_empty() {
        notes.push(file_scope_note(bare, &file_hits));
    }
    if results.is_empty() {
        if notes.is_empty() {
            // Definitions exist but nothing references them: that is a
            // confident "nothing impacted", not a missed lookup.
            notes.push("No impacted symbols found.".into());
        }
        return Ok(QueryResult::from_tiers(results, &[], multi, notes));
    }

    // Impact is always a candidate radius — never High, never fabricated per-hit tiers.
    notes.push(
        "Impact is a candidate blast radius and may over-approximate; verify before edits.".into(),
    );
    let confidence = if multi {
        Confidence::Low
    } else {
        Confidence::Medium
    };
    let tier = if multi {
        ResolutionTier::Mixed
    } else {
        ResolutionTier::Single(2)
    };
    Ok(QueryResult::new(results, confidence, tier, notes))
}

/// Note bridging file-scope impact uses: resolved top-level uses have no
/// enclosing symbol to list, so name their files (capped) and point at
/// `references` for the exact sites.
fn file_scope_note(bare: &str, files: &[String]) -> String {
    const SHOWN: usize = 3;
    let listed: Vec<&str> = files.iter().take(SHOWN).map(String::as_str).collect();
    let suffix = if files.len() > SHOWN {
        format!(", +{} more", files.len() - SHOWN)
    } else {
        String::new()
    };
    format!(
        "Also used at file scope in {}{suffix} (no enclosing symbol); see `references {bare}`.",
        listed.join(", ")
    )
}

/// When every definition shares one `module_path`, return it for precise
/// caller filtering; otherwise `None` (name-based fallback).
fn unique_module(defs: &[Symbol]) -> Option<String> {
    let first = defs.first()?.module_path.clone();
    if defs.iter().all(|d| d.module_path == first) {
        Some(first)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::query_result::Confidence;
    use crate::graph::types::SymbolKind;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn index_facade_indexes_and_finds_definition() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub struct AuthService;\nfn create_order() {}\n",
        )
        .unwrap();

        let mut index = Index::open_in_memory().unwrap();
        let stats = index.index_path(root).unwrap();
        assert_eq!(stats.indexed, 1);

        let defs = index.definition("AuthService").unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].kind, SymbolKind::Struct);
        assert_eq!(defs[0].name, "AuthService");

        let meta = index.definition_with_meta("AuthService").unwrap();
        assert_eq!(meta.confidence, Confidence::High);
    }

    #[test]
    fn dependencies_resolve_rust_file_module() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/mcp")).unwrap();
        fs::create_dir_all(root.join("src/api")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "mod api;\nmod mcp;\n",
        )
        .unwrap();
        fs::write(
            root.join("src/api/mod.rs"),
            "pub struct Token;\n",
        )
        .unwrap();
        fs::write(
            root.join("src/mcp/mod.rs"),
            "use crate::api::Token;\npub fn serve() {}\n",
        )
        .unwrap();

        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        let serve = index.definition("serve").unwrap();
        assert_eq!(serve.len(), 1);
        assert_eq!(serve[0].module_path, "crate::mcp");

        let deps = index.dependencies_with_meta("crate::mcp").unwrap();
        assert!(
            !deps.results.is_empty(),
            "expected imports for crate::mcp, got notes={:?}",
            deps.notes
        );
        let paths: Vec<_> = deps.results.iter().map(|d| d.module_path.as_str()).collect();
        assert!(
            paths.iter().any(|p| *p == "crate::api" || p.starts_with("crate::api")),
            "unexpected deps={paths:?}"
        );
        assert_eq!(deps.confidence, Confidence::High);
    }

    #[test]
    fn missing_definition_is_honest_not_found() {
        // Populated index: a true miss keeps exactly the canonical marker.
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub struct Widget;\n").unwrap();
        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        let meta = index.definition_with_meta("NonexistentSymbolXYZ").unwrap();
        assert!(meta.results.is_empty());
        assert_eq!(meta.confidence, Confidence::High);
        assert_eq!(meta.notes, vec!["No matching symbols found.".to_string()]);
    }

    #[test]
    fn miss_on_empty_index_says_so() {
        // An empty index must not present a bare confident miss: nothing has
        // been indexed, so "not found" would mislead.
        let index = Index::open_in_memory().unwrap();
        let meta = index.definition_with_meta("Anything").unwrap();
        assert!(meta.results.is_empty());
        assert_eq!(meta.notes[0], "No matching symbols found.".to_string());
        assert!(
            meta.notes.iter().any(|n| n.starts_with("Index is empty")),
            "expected empty-index note, got {:?}",
            meta.notes
        );
    }

    #[test]
    fn stale_index_refuses_reads_until_rebuilt() {
        use crate::db::schema;
        use rusqlite::Connection;

        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub struct AuthService;\n").unwrap();

        let mut conn = Connection::open_in_memory().unwrap();
        crate::index::index_repository(root, &mut conn).unwrap();
        assert_eq!(
            schema::index_format_version(&conn),
            schema::INDEX_FORMAT_VERSION
        );

        // Simulate an older writer.
        schema::stamp_index_format(&conn, 0).unwrap();
        let err = definition_with_meta(&conn, "AuthService").unwrap_err();
        assert!(
            matches!(err, crate::error::KeelError::StaleIndex { .. }),
            "expected StaleIndex, got {err}"
        );

        // The next index pass rebuilds transparently and reads work again.
        let stats = crate::index::index_repository(root, &mut conn).unwrap();
        assert_eq!(stats.indexed, 1);
        assert_eq!(
            schema::index_format_version(&conn),
            schema::INDEX_FORMAT_VERSION
        );
        let defs = definition_with_meta(&conn, "AuthService").unwrap();
        assert_eq!(defs.results.len(), 1);
    }

    #[test]
    fn empty_impact_on_existing_symbol_is_honest() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub struct Lone;\n").unwrap();

        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        let meta = index.impact_with_meta("Lone").unwrap();
        assert!(meta.results.is_empty());
        assert_eq!(meta.notes, vec!["No impacted symbols found.".to_string()]);

        // Nonsense names keep the canonical miss.
        let meta = index.impact_with_meta("NonexistentSymbolXYZ123").unwrap();
        assert_eq!(meta.notes[0], "No matching symbols found.".to_string());
    }

    #[test]
    fn near_miss_suggests_canonical_name() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub struct AuthService;\n").unwrap();

        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        // Case-only miss still misses, but recovers with a suggestion.
        let meta = index.definition_with_meta("authservice").unwrap();
        assert!(meta.results.is_empty());
        assert_eq!(meta.confidence, Confidence::High);
        assert_eq!(meta.notes[0], "No matching symbols found.".to_string());
        assert!(
            meta.notes.iter().any(|n| n.contains("`AuthService`")),
            "expected suggestion, got {:?}",
            meta.notes
        );

        // True nonsense stays a clean miss.
        let meta = index.definition_with_meta("NonexistentSymbolXYZ123").unwrap();
        assert_eq!(meta.notes, vec!["No matching symbols found.".to_string()]);
    }

    #[test]
    fn module_scoped_callers_name_unattributed_sites_honestly() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.rs"), "pub fn serve() {}\n").unwrap();
        fs::write(root.join("src/b.rs"), "pub fn serve() {}\n").unwrap();
        fs::write(
            root.join("src/c.rs"),
            "use crate::b;\npub fn run() {\n    b::serve();\n}\n",
        )
        .unwrap();
        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        // Import-backed: the `b::serve()` site resolves to crate::b.
        let meta = index.callers_with_meta_opts("serve", Some("crate::b")).unwrap();
        assert_eq!(meta.results.len(), 1);
        assert_eq!(meta.confidence, Confidence::High);

        // Same site can't resolve to crate::a: definitions matched, so the
        // canonical miss marker would lie — expect the attribution note.
        let meta = index.callers_with_meta_opts("serve", Some("crate::a")).unwrap();
        assert!(meta.results.is_empty());
        assert!(
            meta.notes.iter().any(|n| n.contains("none resolved to module")),
            "got {:?}",
            meta.notes
        );
        assert!(
            !meta.notes.iter().any(|n| n == "No matching symbols found."),
            "canonical marker lies here, got {:?}",
            meta.notes
        );

        let meta = index
            .references_with_meta_opts("serve", Some("crate::a"))
            .unwrap();
        assert!(meta.results.is_empty());
        assert!(
            meta.notes.iter().any(|n| n.contains("none resolved to module")),
            "got {:?}",
            meta.notes
        );
    }

    #[test]
    fn module_scoped_callers_attribute_qualified_sites_per_module() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.rs"), "pub fn serve() {}\n").unwrap();
        fs::write(root.join("src/b.rs"), "pub fn serve() {}\n").unwrap();
        fs::write(
            root.join("src/c.rs"),
            "use crate::{a, b};\npub fn run() {\n    a::serve();\n    b::serve();\n}\n",
        )
        .unwrap();
        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        let meta = index.callers_with_meta_opts("serve", Some("crate::a")).unwrap();
        assert_eq!(meta.results.len(), 1);
        assert_eq!(meta.results[0].start_line, 3);
        assert_eq!(meta.confidence, Confidence::High);

        let meta = index.callers_with_meta_opts("serve", Some("crate::b")).unwrap();
        assert_eq!(meta.results.len(), 1);
        assert_eq!(meta.results[0].start_line, 4);
        assert_eq!(meta.confidence, Confidence::High);
    }

    #[test]
    fn references_include_aliased_uses_with_exact_module() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.ts"), "export function serve() { return 1; }\n").unwrap();
        fs::write(root.join("b.ts"), "export function serve() { return 2; }\n").unwrap();
        fs::write(
            root.join("c.ts"),
            "import { serve as serveA } from \"./a\";\nimport * as b from \"./b\";\nserveA();\nb.serve();\n",
        )
        .unwrap();
        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        let names = |meta: &QueryResult<Reference>| {
            meta.results.iter().map(|r| r.name.clone()).collect::<Vec<_>>()
        };
        // Unflagged: both the alias use and the qualified site.
        let meta = index.references_with_meta("serve").unwrap();
        assert_eq!(names(&meta), vec!["serveA".to_string(), "serve".to_string()]);
        assert!(
            meta.notes.iter().any(|n| n.contains("aliased")),
            "got {:?}",
            meta.notes
        );
        // Flagged: the alias maps to `a`, the qualified site to `b`.
        let meta = index.references_with_meta_opts("serve", Some("a")).unwrap();
        assert_eq!(names(&meta), vec!["serveA".to_string()]);
        let meta = index.references_with_meta_opts("serve", Some("b")).unwrap();
        assert_eq!(names(&meta), vec!["serve".to_string()]);

        let meta = index.callers_with_meta("serve").unwrap();
        assert_eq!(names(&meta), vec!["serveA".to_string(), "serve".to_string()]);
    }

    #[test]
    fn module_scoped_impact_follows_aliased_uses() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("a.ts"), "export function serve() { return 1; }\n").unwrap();
        fs::write(root.join("b.ts"), "export function serve() { return 2; }\n").unwrap();
        fs::write(
            root.join("c.ts"),
            "import { serve as serveA } from \"./a\";\nimport * as b from \"./b\";\nfunction runA() {\n    serveA();\n}\nfunction runB() {\n    b.serve();\n}\n",
        )
        .unwrap();
        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        let names = |meta: &QueryResult<Symbol>| {
            meta.results.iter().map(|s| s.name.clone()).collect::<Vec<_>>()
        };
        let meta_a = index.impact_with_meta_opts("serve", Some("a")).unwrap();
        let names_a = names(&meta_a);
        assert!(names_a.contains(&"runA".to_string()), "got {names_a:?}");

        let meta_b = index.impact_with_meta_opts("serve", Some("b")).unwrap();
        let names_b = names(&meta_b);
        assert!(names_b.contains(&"runB".to_string()), "got {names_b:?}");
        assert!(!names_b.contains(&"runA".to_string()), "got {names_b:?}");
    }

    #[test]
    fn module_scoped_impact_attributes_qualified_sites_per_module() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.rs"), "pub fn serve() {}\n").unwrap();
        fs::write(root.join("src/b.rs"), "pub fn serve() {}\n").unwrap();
        fs::write(
            root.join("src/c.rs"),
            "use crate::{a, b};\npub fn run_a() {\n    a::serve();\n}\npub fn run_b() {\n    b::serve();\n}\n",
        )
        .unwrap();
        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        let names = |meta: &QueryResult<Symbol>| {
            meta.results.iter().map(|s| s.name.clone()).collect::<Vec<_>>()
        };
        let meta_a = index.impact_with_meta_opts("serve", Some("crate::a")).unwrap();
        let names_a = names(&meta_a);
        assert!(names_a.contains(&"run_a".to_string()), "got {names_a:?}");
        assert!(!names_a.contains(&"run_b".to_string()), "got {names_a:?}");

        let meta_b = index.impact_with_meta_opts("serve", Some("crate::b")).unwrap();
        let names_b = names(&meta_b);
        assert!(names_b.contains(&"run_b".to_string()), "got {names_b:?}");
        assert!(!names_b.contains(&"run_a".to_string()), "got {names_b:?}");
    }

    #[test]
    fn defined_never_called_names_empty_site_list() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "pub fn lone() {}\n").unwrap();
        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        let meta = index.callers_with_meta("lone").unwrap();
        assert!(meta.results.is_empty());
        assert_eq!(
            meta.notes,
            vec!["No call sites recorded for `lone`.".to_string()]
        );

        let meta = index.references_with_meta("lone").unwrap();
        assert!(meta.results.is_empty());
        assert_eq!(
            meta.notes,
            vec!["No reference sites recorded for `lone`.".to_string()]
        );
    }

    #[test]
    fn empty_module_is_no_filter() {
        assert_eq!(resolve_symbol_target("serve", Some("")), (None, "serve"));
        assert_eq!(
            resolve_symbol_target("serve", Some("crate::mcp")),
            (Some("crate::mcp"), "serve")
        );
        // An empty module must not shadow a qualified name.
        assert_eq!(
            resolve_symbol_target("crate::mcp::serve", Some("")),
            (Some("crate::mcp"), "serve")
        );
    }

    #[test]
    fn multi_def_notes_omit_impact_wording() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/api")).unwrap();
        fs::create_dir_all(root.join("src/mcp")).unwrap();
        fs::write(root.join("src/lib.rs"), "mod api;\nmod mcp;\n").unwrap();
        fs::write(root.join("src/api/mod.rs"), "pub fn serve() {}\n").unwrap();
        fs::write(root.join("src/mcp/mod.rs"), "pub fn serve() {}\n").unwrap();

        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        let meta = index.definition_with_meta("serve").unwrap();
        assert_eq!(meta.results.len(), 2);
        assert!(!meta
            .notes
            .iter()
            .any(|n| n.contains("over-approximate impact")));
        assert!(meta.notes.iter().any(|n| n.contains("disambiguate")));
    }

    #[test]
    fn definition_accepts_qualified_module_path() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/api")).unwrap();
        fs::create_dir_all(root.join("src/mcp")).unwrap();
        fs::write(root.join("src/lib.rs"), "mod api;\nmod mcp;\n").unwrap();
        fs::write(root.join("src/api/mod.rs"), "pub fn serve() {}\n").unwrap();
        fs::write(root.join("src/mcp/mod.rs"), "pub fn serve() {}\n").unwrap();

        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        let meta = index.definition_with_meta("crate::mcp::serve").unwrap();
        assert_eq!(meta.results.len(), 1);
        assert_eq!(meta.results[0].module_path, "crate::mcp");
        assert_eq!(meta.confidence, Confidence::High);
    }

    #[test]
    fn definition_accepts_explicit_module_filter() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/api")).unwrap();
        fs::create_dir_all(root.join("src/mcp")).unwrap();
        fs::write(root.join("src/lib.rs"), "mod api;\nmod mcp;\n").unwrap();
        fs::write(root.join("src/api/mod.rs"), "pub fn serve() {}\n").unwrap();
        fs::write(root.join("src/mcp/mod.rs"), "pub fn serve() {}\n").unwrap();

        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        let meta = index
            .definition_with_meta_opts("serve", Some("crate::mcp"))
            .unwrap();
        assert_eq!(meta.results.len(), 1);
        assert_eq!(meta.results[0].module_path, "crate::mcp");
    }

    #[test]
    fn definition_module_plus_qualified_name_uses_bare_symbol() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/mcp")).unwrap();
        fs::write(root.join("src/lib.rs"), "mod mcp;\n").unwrap();
        fs::write(root.join("src/mcp/mod.rs"), "pub fn serve() {}\n").unwrap();

        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        // Agents sometimes pass both module and a qualified name.
        let meta = index
            .definition_with_meta_opts("crate::mcp::serve", Some("crate::mcp"))
            .unwrap();
        assert_eq!(meta.results.len(), 1);
        assert_eq!(meta.results[0].name, "serve");
        assert_eq!(meta.results[0].module_path, "crate::mcp");
    }

    #[test]
    fn references_with_module_filter_to_resolved_target() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/api")).unwrap();
        fs::create_dir_all(root.join("src/mcp")).unwrap();
        fs::write(root.join("src/lib.rs"), "mod api;\nmod mcp;\nfn boot() { mcp::serve(); }\n").unwrap();
        fs::write(root.join("src/api/mod.rs"), "pub fn serve() {}\n").unwrap();
        fs::write(
            root.join("src/mcp/mod.rs"),
            "pub fn serve() {}\nfn other() { serve(); }\n",
        )
        .unwrap();

        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        let all = index.references_with_meta("serve").unwrap();
        assert!(!all.results.is_empty());

        let mcp_only = index
            .references_with_meta_opts("serve", Some("crate::mcp"))
            .unwrap();
        // Every retained ref must resolve toward crate::mcp (same-module or import).
        assert!(
            !mcp_only.results.is_empty(),
            "expected at least the same-module call in mcp"
        );
        assert!(
            mcp_only.confidence == Confidence::High || mcp_only.confidence == Confidence::Medium,
            "narrowed refs should not be low-confidence theater; got {:?}",
            mcp_only.confidence
        );
        // api-only seed should not include mcp-internal same-module call if filtered.
        let api_only = index
            .references_with_meta_opts("serve", Some("crate::api"))
            .unwrap();
        assert!(
            api_only.results.len() < all.results.len()
                || api_only.results.is_empty()
                || mcp_only.results != api_only.results,
            "module filter should change the reference set"
        );
    }

    #[test]
    fn impact_with_module_is_candidate_medium_not_fake_high() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "fn a() {}\nfn b() { a(); }\nfn c() { b(); }\n",
        )
        .unwrap();

        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        let meta = index.impact_with_meta("a").unwrap();
        assert!(!meta.results.is_empty());
        assert_eq!(meta.confidence, Confidence::Medium);
        assert!(meta.notes.iter().any(|n| n.contains("candidate blast radius")));
    }

    #[test]
    fn impact_qualified_does_not_expand_other_same_name_def() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/api")).unwrap();
        fs::create_dir_all(root.join("src/mcp")).unwrap();
        fs::write(root.join("src/lib.rs"), "mod api;\nmod mcp;\n").unwrap();
        fs::write(root.join("src/api/mod.rs"), "pub fn serve() {}\n").unwrap();
        fs::write(
            root.join("src/mcp/mod.rs"),
            "pub fn serve() {}\nfn other() { serve(); }\n",
        )
        .unwrap();

        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        let api_impact = index
            .impact_with_meta_opts("serve", Some("crate::api"))
            .unwrap();
        assert!(
            api_impact.results.is_empty(),
            "api::serve is unused; got {:?}",
            api_impact.results.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        let mcp_impact = index.impact_with_meta("crate::mcp::serve").unwrap();
        assert!(
            mcp_impact.results.iter().any(|s| s.name == "other"),
            "mcp::serve should impact same-module other; got {:?}",
            mcp_impact.results.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert_eq!(mcp_impact.confidence, Confidence::Medium);
        // Bare-name impact would seed both defs; qualified must not pull api-only noise
        // and must not report High.
        assert_ne!(mcp_impact.confidence, Confidence::High);
    }

    #[test]
    fn implementations_narrow_by_module_and_qualified_name() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/a.rs"),
            "pub trait Store {}\npub struct A;\nimpl Store for A {}\n",
        )
        .unwrap();
        fs::write(
            root.join("src/b.rs"),
            "pub trait Store {}\npub struct B;\nimpl Store for B {}\n",
        )
        .unwrap();
        fs::write(root.join("src/main.rs"), "mod a;\nmod b;\nfn main() {}\n").unwrap();

        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        // Bare: both, with a same-name warning.
        let both = index.implementations_with_meta("Store").unwrap();
        assert_eq!(both.results.len(), 2);
        assert!(
            both.notes.iter().any(|n| n.contains("Multiple traits")),
            "got {:?}",
            both.notes
        );

        // Module and qualified narrowing agree on A.
        for qr in [
            index
                .implementations_with_meta_opts("Store", Some("crate::a"))
                .unwrap(),
            index.implementations_with_meta("crate::a::Store").unwrap(),
        ] {
            let types: Vec<&str> = qr
                .results
                .iter()
                .map(|r| r.type_name.as_str())
                .collect();
            assert_eq!(types, vec!["A"]);
        }

        // Wrong module: honest miss, not B.
        let miss = index
            .implementations_with_meta_opts("Store", Some("crate::zzz"))
            .unwrap();
        assert!(miss.results.is_empty());
        assert!(
            miss.notes.iter().any(|n| n.contains("No definition")),
            "got {:?}",
            miss.notes
        );
    }

    #[test]
    fn references_and_impact_follow_rust_reexports() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/a.rs"), "pub fn alpha() -> i32 { 1 }\n").unwrap();
        fs::write(root.join("src/b.rs"), "pub use crate::a::alpha;\n").unwrap();
        fs::write(
            root.join("src/c.rs"),
            "use crate::b::alpha;\npub fn beta() -> i32 { alpha() }\n",
        )
        .unwrap();
        fs::write(
            root.join("src/main.rs"),
            "mod a;\nmod b;\nmod c;\nfn main() {}\n",
        )
        .unwrap();

        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        let refs = index.references_with_meta("alpha").unwrap();
        assert_eq!(refs.results.len(), 1);
        assert_eq!(refs.results[0].file.to_string_lossy(), "src/c.rs");

        let hit = index.impact_with_meta("alpha").unwrap();
        let names: Vec<&str> = hit.results.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["beta"]);
    }

    #[test]
    fn outline_lists_file_symbols_in_source_order() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub struct AuthService;\nfn create_order() {}\nfn zebra() {}\n",
        )
        .unwrap();

        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        let meta = index.outline_with_meta("src/lib.rs").unwrap();
        let names: Vec<&str> = meta.results.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["AuthService", "create_order", "zebra"]);
        assert_eq!(meta.confidence, Confidence::High);
    }

    #[test]
    fn unused_reports_uncalled_functions_and_skips_entry_points() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("main.rs"),
            "fn live() {}\nfn dead() {}\nfn main() { live(); }\nfn dead_top() { dead_mid(); }\nfn dead_mid() { dead_leaf(); }\nfn dead_leaf() {}\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n",
        )
        .unwrap();
        fs::create_dir_all(root.join("tests")).unwrap();
        fs::write(root.join("tests/helpers.rs"), "fn helper() {}\n").unwrap();

        fs::write(
            root.join("pkg.go"),
            "package main\n\nfunc init() {}\nfunc BenchmarkX(b int) {}\nfunc dead_go() {}\nfunc main() {}\n",
        )
        .unwrap();

        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        let meta = index.unused_with_meta(None).unwrap();
        let names: Vec<&str> = meta.results.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["dead", "dead_top", "dead_go"]);
        assert_eq!(meta.confidence, Confidence::Low);
        assert!(
            meta.notes.iter().any(|n| n.contains("Candidates only")),
            "missing candidate caveat: {:?}",
            meta.notes
        );

        // Transitive pass pulls in the chain below dead_top; the live
        // chain stays out.
        let trans = index.unused_with_meta_opts(None, true).unwrap();
        let names: Vec<&str> = trans.results.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["dead", "dead_top", "dead_mid", "dead_leaf", "dead_go"]
        );
        assert!(
            trans.notes.iter().any(|n| n.contains("Transitive pass")),
            "missing transitive note: {:?}",
            trans.notes
        );

        // Scoping to a file with no dead code reports an honest empty sweep.
        let scoped = index.unused_with_meta(Some("tests/helpers.rs")).unwrap();
        assert!(scoped.results.is_empty());
        assert_eq!(scoped.confidence, Confidence::High);
    }

    #[test]
    fn outline_accepts_dot_slash_and_absolute_paths() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "fn solo() {}\n").unwrap();

        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        for raw in [
            "./src/lib.rs".to_string(),
            root.join("src/lib.rs").display().to_string(),
        ] {
            let meta = index.outline_with_meta(&raw).unwrap();
            assert_eq!(meta.results.len(), 1, "for input {raw}");
            assert_eq!(meta.results[0].name, "solo");
        }
    }

    #[test]
    fn outline_miss_suggests_files_and_names_empty_files() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "fn solo() {}\n").unwrap();
        fs::write(root.join("src/empty.py"), "# nothing here\n").unwrap();

        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        let meta = index.outline_with_meta("lib.rs").unwrap();
        assert!(meta.results.is_empty());
        assert!(
            meta.notes.iter().any(|n| n.contains("No indexed file")),
            "got {:?}",
            meta.notes
        );
        assert!(
            meta.notes.iter().any(|n| n.contains("`src/lib.rs`")),
            "expected file suggestion, got {:?}",
            meta.notes
        );

        let meta = index.outline_with_meta("src/empty.py").unwrap();
        assert!(meta.results.is_empty());
        assert_eq!(
            meta.notes,
            vec!["File `src/empty.py` is indexed but declares no symbols.".to_string()]
        );
    }

    #[test]
    fn outline_accepts_module_and_directory_targets() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/pkg")).unwrap();
        fs::write(root.join("src/pkg/a.py"), "def alpha():\n    pass\n").unwrap();
        fs::write(root.join("src/pkg/b.py"), "def beta():\n    pass\n").unwrap();
        fs::write(root.join("src/top.py"), "def top():\n    pass\n").unwrap();

        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        // Directory: file order, then source order.
        let meta = index.outline_with_meta("src/pkg").unwrap();
        let names: Vec<&str> = meta.results.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert_eq!(meta.confidence, Confidence::High);

        // Parent module covers the subtree; the outside file stays out.
        let meta = index.outline_with_meta("src.pkg").unwrap();
        let names: Vec<&str> = meta.results.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta"]);

        // Bare symbol names are still a miss, not a definition lookup.
        let meta = index.outline_with_meta("alpha").unwrap();
        assert!(meta.results.is_empty());
        assert!(
            meta.notes.iter().any(|n| n.contains("No indexed file")),
            "got {:?}",
            meta.notes
        );
    }

    fn search_fixture() -> (tempfile::TempDir, Index) {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub struct AuthService;\nfn create_order() {}\nfn cancel_order() {}\nfn zebra() {}\n",
        )
        .unwrap();
        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();
        (dir, index)
    }

    #[test]
    fn search_ranks_exact_prefix_then_substring() {
        let (_dir, index) = search_fixture();

        let meta = index.search_with_meta("order", 50).unwrap();
        let names: Vec<&str> = meta.results.iter().map(|s| s.name.as_str()).collect();
        // No exact/prefix hit: location order, partial-only note.
        assert_eq!(names, vec!["create_order", "cancel_order"]);
        assert!(
            meta.notes.iter().any(|n| n.contains("No exact match")),
            "got {:?}",
            meta.notes
        );

        let meta = index.search_with_meta("AuthService", 50).unwrap();
        assert_eq!(meta.results.len(), 1);
        assert_eq!(meta.results[0].name, "AuthService");
        assert_eq!(meta.confidence, Confidence::High);
        assert!(meta.notes.is_empty(), "got {:?}", meta.notes);

        let meta = index.search_with_meta("auth", 50).unwrap();
        assert_eq!(meta.results.len(), 1);
        assert!(meta.notes.iter().any(|n| n.contains("No exact match")));
    }

    #[test]
    fn search_respects_limit_and_reports_truncation() {
        let (_dir, index) = search_fixture();
        let meta = index.search_with_meta("e", 2).unwrap();
        assert_eq!(meta.results.len(), 2);
        assert!(
            meta.notes.iter().any(|n| n.contains("first 2 matches")),
            "got {:?}",
            meta.notes
        );
    }

    #[test]
    fn search_miss_and_empty_pattern_stay_honest() {
        let (_dir, index) = search_fixture();
        let meta = index.search_with_meta("zzz_nope", 50).unwrap();
        assert!(meta.results.is_empty());
        assert!(
            meta.notes.iter().any(|n| n.contains("No symbols matching")),
            "got {:?}",
            meta.notes
        );
        let meta = index.search_with_meta("", 50).unwrap();
        assert!(meta.results.is_empty());
        assert_eq!(meta.notes, vec!["Empty search pattern.".to_string()]);
    }

    #[test]
    fn dependents_reports_hits_and_honest_misses() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/b.rs"), "pub fn f() {}\n").unwrap();
        fs::write(
            root.join("src/a.rs"),
            "use crate::b::f;\npub fn g() {\n    f();\n}\n",
        )
        .unwrap();
        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        let meta = index.dependents_with_meta("crate::b").unwrap();
        let modules: Vec<&str> = meta
            .results
            .iter()
            .map(|d| d.module_path.as_str())
            .collect();
        assert!(modules.contains(&"crate::a"), "got {modules:?}");
        assert_eq!(meta.confidence, Confidence::High);

        let meta = index.dependents_with_meta("crate::a").unwrap();
        assert!(meta.results.is_empty());
        assert!(
            meta.notes.iter().any(|n| n.contains("no indexed module")),
            "got {:?}",
            meta.notes
        );

        let meta = index.dependents_with_meta("NopeXYZ").unwrap();
        assert!(meta.results.is_empty());
        assert!(
            meta.notes.iter().any(|n| n.contains("No indexed files")),
            "got {:?}",
            meta.notes
        );
    }

    #[test]
    fn search_treats_like_wildcards_literally() {
        let (_dir, index) = search_fixture();
        // `%`/`_` must not act as wildcards: no symbol contains them.
        let meta = index.search_with_meta("%_%", 50).unwrap();
        assert!(meta.results.is_empty());
    }
}
