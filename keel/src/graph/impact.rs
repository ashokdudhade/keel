//! Transitive impact analysis: who (directly or indirectly) references a name.

use crate::db::queries;
use crate::error::Result;
use crate::graph::resolve;
use crate::graph::types::{Reference, ReferenceKind, Symbol};
use rusqlite::Connection;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;

/// Return the transitive set of symbols that reference `name`.
///
/// Seeds the worklist from every definition of `name` (or the bare name when
/// none exist). See [`find_impact_from_defs`] to seed a specific identity.
///
/// Expansion uses qualified identities (`module_path::name` when module is
/// non-empty). References are accepted only when
/// [`resolve::resolve_definition_ranked`] from the reference's file yields an
/// [`resolve::acceptable_top_match`] whose identity equals the worklist target,
/// except tier-3 (no import, other module) method calls, which need stronger
/// evidence: attribute access on an unknown receiver collides with unrelated
/// free functions too often.
pub fn find_impact(conn: &Connection, name: &str) -> Result<Vec<Symbol>> {
    let defs = queries::find_definition(conn, name)?;
    if defs.is_empty() {
        find_impact_from_identities(conn, &[name.to_string()])
    } else {
        find_impact_from_defs(conn, &defs)
    }
}

/// Impact seeded only from the given definition symbols (qualified identities).
///
/// Use this when the caller has already disambiguated (module / qualified name)
/// so bare-name collisions do not expand unrelated same-named definitions.
pub fn find_impact_from_defs(conn: &Connection, defs: &[Symbol]) -> Result<Vec<Symbol>> {
    Ok(find_impact_from_defs_with_file_hits(conn, defs)?.0)
}

/// Like [`find_impact_from_defs`], additionally returning the sorted files
/// whose top-level (file-scope) code references the target: those uses
/// resolve but have no enclosing symbol to list, so callers surface them
/// as a note instead of dropping them silently.
pub fn find_impact_from_defs_with_file_hits(
    conn: &Connection,
    defs: &[Symbol],
) -> Result<(Vec<Symbol>, Vec<String>)> {
    let identities: Vec<String> = defs.iter().map(symbol_identity).collect();
    find_impact_from_identities_with_file_hits(conn, &identities)
}

fn find_impact_from_identities(conn: &Connection, seed_ids: &[String]) -> Result<Vec<Symbol>> {
    Ok(find_impact_from_identities_with_file_hits(conn, seed_ids)?.0)
}

/// Per-run memo table for the impact BFS. Hot names reappear across
/// worklist items (every same-named container re-seeds the same lookups),
/// so uncached runs issue quadratic SQL on large repos. The cache is
/// dropped with the query; the index is read-only here, so entries cannot
/// go stale mid-run.
/// Shared reference rows (hot names run to tens of thousands of rows).
type SharedReferences = Rc<Vec<Reference>>;
/// Shared ranked definitions `(tier, symbol)`.
type SharedRanked = Rc<Vec<(u8, Symbol)>>;
/// Shared `(module_path, alias)` import rows for one file.
type SharedImports = Rc<Vec<(String, Option<String>)>>;
/// Shared symbol rows.
type SharedSymbols = Rc<Vec<Symbol>>;
/// Two-level memo keyed by single strings, so lookups never allocate.
type NestedMemo<T> = HashMap<String, HashMap<String, T>>;

struct ImpactCache<'a> {
    conn: &'a Connection,
    // The BFS visits millions of references on large repos: nested
    // single-string keys keep lookups allocation-free, and `Rc` values
    // share rows instead of cloning per worklist item.
    references: HashMap<String, SharedReferences>,
    ranked: NestedMemo<SharedRanked>,
    imports: HashMap<String, SharedImports>,
    alias_uses: NestedMemo<SharedReferences>,
    qualified_defs: NestedMemo<SharedSymbols>,
    defs: HashMap<String, SharedSymbols>,
}

impl<'a> ImpactCache<'a> {
    fn new(conn: &'a Connection) -> Self {
        Self {
            conn,
            references: HashMap::new(),
            ranked: HashMap::new(),
            imports: HashMap::new(),
            alias_uses: HashMap::new(),
            qualified_defs: HashMap::new(),
            defs: HashMap::new(),
        }
    }

    fn references(&mut self, name: &str) -> Result<SharedReferences> {
        if let Some(hit) = self.references.get(name) {
            return Ok(hit.clone());
        }
        let rows = Rc::new(queries::find_references(self.conn, name)?);
        self.references.insert(name.to_string(), rows.clone());
        Ok(rows)
    }

    fn ranked(&mut self, name: &str, from: &str) -> Result<SharedRanked> {
        if let Some(inner) = self.ranked.get(name) {
            if let Some(hit) = inner.get(from) {
                return Ok(hit.clone());
            }
        }
        let rows = Rc::new(resolve::resolve_definition_ranked(
            self.conn, name, from,
        )?);
        self.ranked
            .entry(name.to_string())
            .or_default()
            .insert(from.to_string(), rows.clone());
        Ok(rows)
    }

    fn imports(&mut self, from: &str) -> Result<SharedImports> {
        if let Some(hit) = self.imports.get(from) {
            return Ok(hit.clone());
        }
        let rows = Rc::new(queries::imports_for_file(self.conn, from)?);
        self.imports.insert(from.to_string(), rows.clone());
        Ok(rows)
    }

    fn alias_uses(&mut self, name: &str, module: &str) -> Result<SharedReferences> {
        if let Some(inner) = self.alias_uses.get(name) {
            if let Some(hit) = inner.get(module) {
                return Ok(hit.clone());
            }
        }
        let rows = Rc::new(resolve::find_alias_uses(self.conn, name, Some(module))?);
        self.alias_uses
            .entry(name.to_string())
            .or_default()
            .insert(module.to_string(), rows.clone());
        Ok(rows)
    }

    fn qualified_defs(&mut self, module: &str, name: &str) -> Result<SharedSymbols> {
        if let Some(inner) = self.qualified_defs.get(module) {
            if let Some(hit) = inner.get(name) {
                return Ok(hit.clone());
            }
        }
        let rows = Rc::new(queries::find_definition_by_qualified(
            self.conn, module, name,
        )?);
        self.qualified_defs
            .entry(module.to_string())
            .or_default()
            .insert(name.to_string(), rows.clone());
        Ok(rows)
    }

    fn defs(&mut self, name: &str) -> Result<SharedSymbols> {
        if let Some(hit) = self.defs.get(name) {
            return Ok(hit.clone());
        }
        let rows = Rc::new(queries::find_definition(self.conn, name)?);
        self.defs.insert(name.to_string(), rows.clone());
        Ok(rows)
    }
}

fn find_impact_from_identities_with_file_hits(
    conn: &Connection,
    seed_ids: &[String],
) -> Result<(Vec<Symbol>, Vec<String>)> {
    let mut visited: HashSet<String> = HashSet::new();
    let mut worklist: BTreeSet<String> = BTreeSet::new();

    for id in seed_ids {
        visited.insert(id.clone());
        worklist.insert(id.clone());
    }

    let mut impact: Vec<Symbol> = Vec::new();
    let mut seen_symbols: HashSet<(String, String, String, u32, u32)> = HashSet::new();
    let mut file_hits: BTreeSet<String> = BTreeSet::new();
    let mut cache = ImpactCache::new(conn);

    while let Some(current_id) = pop_front(&mut worklist) {
        let current_name = bare_name(&current_id).to_string();
        for reference in cache.references(&current_name)?.iter() {
            let from = reference.file.to_string_lossy().into_owned();
            let ranked = cache.ranked(&current_name, &from)?;
            let imports = cache.imports(&from)?;
            if !resolves_to_target(&ranked, &current_id, reference, &imports) {
                continue;
            }

            let container = reference.container.as_str();
            if container.is_empty() || visited.contains(container) {
                continue;
            }
            visited.insert(container.to_string());
            worklist.insert(container.to_string());

            if looks_like_file_path(container) {
                file_hits.insert(container.to_string());
            }
            push_container_symbols(&mut cache, container, &mut impact, &mut seen_symbols)?;
        }
        // Uses through import aliases (`serveA()` for `a::serve`): the alias
        // row already pins the exact module, so no further resolution check
        // is needed. Bare-name seeds skip this (no module to verify against).
        if let Some((target_module, _)) = current_id.rsplit_once("::") {
            for reference in cache.alias_uses(&current_name, target_module)?.iter() {
                let container = reference.container.as_str();
                if container.is_empty() || visited.contains(container) {
                    continue;
                }
                visited.insert(container.to_string());
                worklist.insert(container.to_string());

                if looks_like_file_path(container) {
                    file_hits.insert(container.to_string());
                }
                push_container_symbols(&mut cache, container, &mut impact, &mut seen_symbols)?;
            }
        }
    }

    impact.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.start_line.cmp(&b.start_line))
            .then_with(|| a.start_col.cmp(&b.start_col))
    });
    Ok((impact, file_hits.into_iter().collect()))
}

fn push_container_symbols(
    cache: &mut ImpactCache,
    container: &str,
    impact: &mut Vec<Symbol>,
    seen_symbols: &mut HashSet<(String, String, String, u32, u32)>,
) -> Result<()> {
    // Qualified container (`module::name`) is always a symbol identity:
    // file paths never contain `::`. The module half may be dotted or
    // slashed (Python `src.b`, TypeScript `src/auth`) — that must not be
    // mistaken for a file path, or impact goes blind outside Rust.
    if let Some((module, name)) = container.rsplit_once("::") {
        // Nested scopes (`src.b::Beta::run`): methods are stored under the
        // file module, so retry with enclosing scopes stripped inside-out
        // (most specific first). Every hit is still an exact module+name
        // match against extracted symbols — no guessing.
        let mut scope = Some(module);
        while let Some(m) = scope {
            let qualified = cache.qualified_defs(m, name)?;
            if !qualified.is_empty() {
                for sym in qualified.iter() {
                    insert_impact_symbol(sym.clone(), impact, seen_symbols);
                }
                return Ok(());
            }
            scope = m.rsplit_once("::").map(|(parent, _)| parent);
        }
        for sym in cache.defs(name)?.iter() {
            if symbol_identity(sym) == container {
                insert_impact_symbol(sym.clone(), impact, seen_symbols);
            }
        }
        return Ok(());
    }

    if looks_like_file_path(container) {
        // File-path container (empty extraction scope): no single symbol to add.
        return Ok(());
    }

    let bare = bare_name(container).to_string();
    for sym in cache.defs(&bare)?.iter() {
        insert_impact_symbol(sym.clone(), impact, seen_symbols);
    }
    Ok(())
}

fn insert_impact_symbol(
    sym: Symbol,
    impact: &mut Vec<Symbol>,
    seen_symbols: &mut HashSet<(String, String, String, u32, u32)>,
) {
    let key = (
        sym.name.clone(),
        sym.module_path.clone(),
        sym.file.to_string_lossy().into_owned(),
        sym.start_line,
        sym.start_col,
    );
    if seen_symbols.insert(key) {
        impact.push(sym);
    }
}

fn symbol_identity(sym: &Symbol) -> String {
    if sym.module_path.is_empty() {
        sym.name.clone()
    } else {
        format!("{}::{}", sym.module_path, sym.name)
    }
}

fn bare_name(identity: &str) -> &str {
    identity.rsplit("::").next().unwrap_or(identity)
}

fn looks_like_file_path(s: &str) -> bool {
    s.contains('/') || s.contains('\\') || Path::new(s).extension().is_some()
}

fn resolves_to_target(
    ranked: &[(u8, Symbol)],
    target_id: &str,
    reference: &crate::graph::types::Reference,
    imports: &[(String, Option<String>)],
) -> bool {
    // Same acceptance as [`resolve::acceptable_top_match`], but the tier is
    // kept so the tier gates below can see it.
    let (tier, sym) = match ranked {
        [] => return false,
        [(tier, sym)] => (tier, sym),
        [(tier, sym), ..] => (tier, sym),
    };
    // Stored qualifiers (`mcp` in `mcp::serve`) decide module ties before the
    // tier gates: when several modules tie at the top rank, only the module
    // the qualifier uniquely selects counts. This runs first because namespace
    // imports (which the tier computation can't see) legitimately tie below
    // tier 2 — there the qualifier must resolve through an import row. Ties
    // without a deciding qualifier fall through to the gates below.
    if matches!(
        reference.kind,
        ReferenceKind::Path | ReferenceKind::Method
    ) && !reference.qualifier.is_empty()
    {
        if let Some((target_module, _)) = target_id.rsplit_once("::") {
            let tied: Vec<&(u8, Symbol)> = ranked
                .iter()
                .take_while(|(t, _)| *t == *tier)
                .collect();
            let distinct = {
                let mut modules: Vec<&str> =
                    tied.iter().map(|(_, s)| s.module_path.as_str()).collect();
                modules.sort_unstable();
                modules.dedup();
                modules.len()
            };
            if distinct > 1 {
                let strict = *tier > 2;
                let matches = |module: &str| {
                    if strict {
                        resolve::qualifier_matches_via_import(
                            &reference.qualifier,
                            module,
                            imports,
                        )
                    } else {
                        resolve::qualifier_matches_module(
                            &reference.qualifier,
                            module,
                            imports,
                        )
                    }
                };
                let selects_target = matches(target_module);
                let selects_other = tied
                    .iter()
                    .any(|(_, s)| s.module_path != target_module && matches(&s.module_path));
                let target_tied = tied
                    .iter()
                    .any(|(_, s)| s.module_path == target_module);
                if selects_target && !selects_other && target_tied {
                    return true;
                }
                if selects_target || selects_other {
                    // The qualifier speaks, but not uniquely for the target.
                    return false;
                }
                // Qualifier matches nothing: fall through to tier gates.
            }
        }
    }
    // Member reads (`c.port`) and method calls on locals
    // (`c.describe()`) name the module only through the receiver's
    // type, which the index can't see: the import binds `Config`, not
    // `port`/`describe`. When nothing resolves precisely and the
    // target module is imported by the use file (under any alias),
    // attribute there. Singleton ranks additionally require the
    // qualifier to resolve through an import row to the target module
    // (`_.map()` via `import * as _`): an unrelated import of the
    // target module plus an unknown receiver (`client.get()`) is
    // coincidence, not evidence, and still falls to the tier gates.
    if matches!(
        reference.kind,
        ReferenceKind::Value | ReferenceKind::Method
    ) && !reference.qualifier.is_empty()
        && *tier > 2
    {
        if let Some((target_module, _)) = target_id.rsplit_once("::") {
            let target_ranked = ranked.iter().any(|(_, s)| symbol_identity(s) == target_id);
            let module_imported = imports
                .iter()
                .any(|(p, _)| resolve::import_row_matches_module(p, target_module));
            // A qualifier resolving through an import row to another
            // ranked module is precise evidence: it beats the fallback.
            let selects_other = ranked.iter().any(|(_, s)| {
                symbol_identity(s) != target_id
                    && s.module_path != target_module
                    && resolve::qualifier_matches_via_import(
                        &reference.qualifier,
                        &s.module_path,
                        imports,
                    )
            });
            let singleton_precise = ranked.len() > 1
                || resolve::qualifier_matches_via_import(
                    &reference.qualifier,
                    target_module,
                    imports,
                );
            if target_ranked && module_imported && !selects_other && singleton_precise {
                return true;
            }
        }
    }
    if *tier > 2 && ranked.len() > 1 {
        return false;
    }
    // Tier-3 singleton fallback (cross-file, no import): method calls on an
    // unknown receiver routinely collide with unrelated free functions
    // (`client.get()` vs the only free `get`), so they need tier ≤ 2
    // evidence. Same-file and imported method calls are unaffected.
    if *tier == 3 && reference.kind == ReferenceKind::Method {
        return false;
    }
    let id = symbol_identity(sym);
    if id == target_id {
        return true;
    }
    // Bare-name seed (no definitions found): accept precise/unique defs of that name.
    if !target_id.contains("::") && sym.name == target_id {
        return true;
    }
    false
}

fn pop_front(worklist: &mut BTreeSet<String>) -> Option<String> {
    let next = worklist.iter().next()?.clone();
    worklist.remove(&next);
    Some(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{queries, schema};
    use crate::graph::types::{FileNode, Import, Reference, ReferenceKind, SymbolKind};
    use std::path::PathBuf;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        conn
    }

    /// Chain: `a` ← `b` ← `c` (b calls a, c calls b).
    fn fixture_call_chain(conn: &Connection) {
        let f = queries::insert_file(
            conn,
            &FileNode {
                path: PathBuf::from("src/lib.rs"),
                content_hash: "h".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            conn,
            f,
            &[
                Symbol {
                    name: "a".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: 1,
                    start_col: 1,
                    module_path: "crate".into(),
                    container: String::new(),
                },
                Symbol {
                    name: "b".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: 2,
                    start_col: 1,
                    module_path: "crate".into(),
                    container: String::new(),
                },
                Symbol {
                    name: "c".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: 3,
                    start_col: 1,
                    module_path: "crate".into(),
                    container: String::new(),
                },
                Symbol {
                    name: "lonely".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: 4,
                    start_col: 1,
                    module_path: "crate".into(),
                    container: String::new(),
                },
            ],
        )
        .unwrap();
        queries::insert_references(
            conn,
            f,
            &[
                Reference {
                    name: "a".into(),
                    file: PathBuf::new(),
                    start_line: 2,
                    start_col: 10,
                    kind: ReferenceKind::Call,
                    container: "crate::b".into(),
                    qualifier: String::new(),
                },
                Reference {
                    name: "b".into(),
                    file: PathBuf::new(),
                    start_line: 3,
                    start_col: 10,
                    kind: ReferenceKind::Call,
                    container: "crate::c".into(),
                    qualifier: String::new(),
                },
            ],
        )
        .unwrap();
    }

    /// Dotted modules (Python style): `beta` in `src.b` calls `alpha` in `src.a`.
    fn fixture_dotted_modules(conn: &Connection) {
        use crate::graph::types::Import;
        let a = queries::insert_file(
            conn,
            &FileNode {
                path: PathBuf::from("src/a.py"),
                content_hash: "ha".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            conn,
            a,
            &[Symbol {
                name: "alpha".into(),
                kind: SymbolKind::Function,
                file: PathBuf::from("src/a.py"),
                start_line: 1,
                start_col: 1,
                module_path: "src.a".into(),
                container: String::new(),
            }],
        )
        .unwrap();
        let b = queries::insert_file(
            conn,
            &FileNode {
                path: PathBuf::from("src/b.py"),
                content_hash: "hb".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            conn,
            b,
            &[Symbol {
                name: "beta".into(),
                kind: SymbolKind::Function,
                file: PathBuf::from("src/b.py"),
                start_line: 3,
                start_col: 1,
                module_path: "src.b".into(),
                container: String::new(),
            }],
        )
        .unwrap();
        queries::insert_imports(
            conn,
            b,
            &[Import {
                module_path: "src.a".into(),
                alias: None,
                file: PathBuf::from("src/b.py"),
            }],
        )
        .unwrap();
        queries::insert_references(
            conn,
            b,
            &[Reference {
                name: "alpha".into(),
                file: PathBuf::from("src/b.py"),
                start_line: 4,
                start_col: 12,
                kind: ReferenceKind::Call,
                container: "src.b::beta".into(),
                qualifier: String::new(),
            }],
        )
        .unwrap();
    }

    #[test]
    fn dotted_module_containers_resolve() {
        let conn = setup();
        fixture_dotted_modules(&conn);
        let hit = find_impact(&conn, "alpha").unwrap();
        let names: Vec<&str> = hit.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["beta"]);
    }

    #[test]
    fn nested_class_containers_strip_to_file_module() {
        use crate::graph::types::Import;
        let conn = setup();
        let a = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("src/a.py"),
                content_hash: "ha".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            &conn,
            a,
            &[Symbol {
                name: "alpha".into(),
                kind: SymbolKind::Function,
                file: PathBuf::from("src/a.py"),
                start_line: 1,
                start_col: 1,
                module_path: "src.a".into(),
                container: String::new(),
            }],
        )
        .unwrap();
        let b = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("src/b.py"),
                content_hash: "hb".into(),
            },
        )
        .unwrap();
        // Method stored flat under the file module, as the extractor writes it.
        queries::insert_symbols(
            &conn,
            b,
            &[Symbol {
                name: "run".into(),
                kind: SymbolKind::Function,
                file: PathBuf::from("src/b.py"),
                start_line: 4,
                start_col: 5,
                module_path: "src.b".into(),
                container: String::new(),
            }],
        )
        .unwrap();
        queries::insert_imports(
            &conn,
            b,
            &[Import {
                module_path: "src.a".into(),
                alias: None,
                file: PathBuf::from("src/b.py"),
            }],
        )
        .unwrap();
        queries::insert_references(
            &conn,
            b,
            &[Reference {
                name: "alpha".into(),
                file: PathBuf::from("src/b.py"),
                start_line: 5,
                start_col: 16,
                kind: ReferenceKind::Call,
                container: "src.b::Beta::run".into(),
                qualifier: String::new(),
            }],
        )
        .unwrap();
        let hit = find_impact(&conn, "alpha").unwrap();
        let names: Vec<&str> = hit.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["run"]);
    }

    /// Cycle: `x` → `y` → `z` → `x` (and a mutual edge `x` ↔ `y`).
    fn fixture_cycle(conn: &Connection) {
        let f = queries::insert_file(
            conn,
            &FileNode {
                path: PathBuf::from("src/cycle.rs"),
                content_hash: "hc".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            conn,
            f,
            &[
                Symbol {
                    name: "x".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: 1,
                    start_col: 1,
                    module_path: "crate".into(),
                    container: String::new(),
                },
                Symbol {
                    name: "y".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: 2,
                    start_col: 1,
                    module_path: "crate".into(),
                    container: String::new(),
                },
                Symbol {
                    name: "z".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: 3,
                    start_col: 1,
                    module_path: "crate".into(),
                    container: String::new(),
                },
            ],
        )
        .unwrap();
        queries::insert_references(
            conn,
            f,
            &[
                Reference {
                    name: "x".into(),
                    file: PathBuf::new(),
                    start_line: 2,
                    start_col: 10,
                    kind: ReferenceKind::Call,
                    container: "crate::y".into(),
                    qualifier: String::new(),
                },
                Reference {
                    name: "y".into(),
                    file: PathBuf::new(),
                    start_line: 3,
                    start_col: 10,
                    kind: ReferenceKind::Call,
                    container: "crate::z".into(),
                    qualifier: String::new(),
                },
                Reference {
                    name: "z".into(),
                    file: PathBuf::new(),
                    start_line: 1,
                    start_col: 10,
                    kind: ReferenceKind::Call,
                    container: "crate::x".into(),
                    qualifier: String::new(),
                },
                Reference {
                    name: "y".into(),
                    file: PathBuf::new(),
                    start_line: 1,
                    start_col: 20,
                    kind: ReferenceKind::Call,
                    container: "crate::x".into(),
                    qualifier: String::new(),
                },
            ],
        )
        .unwrap();
    }

    #[test]
    fn find_impact_returns_transitive_callers() {
        let conn = setup();
        fixture_call_chain(&conn);

        let impact = find_impact(&conn, "a").unwrap();
        let names: Vec<&str> = impact.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["b", "c"]);
    }

    #[test]
    fn find_impact_no_callers_returns_empty() {
        let conn = setup();
        fixture_call_chain(&conn);

        let impact = find_impact(&conn, "lonely").unwrap();
        assert!(impact.is_empty(), "expected empty, got {impact:?}");
    }

    #[test]
    fn file_scope_uses_report_as_file_hits_not_symbols() {
        let conn = setup();
        // `alpha` defined in a.py, called only at `main.py` top level (the
        // container is the file itself): no symbol to list, but the use
        // resolves and must surface as a file hit.
        let a = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("pkg/a.py"),
                content_hash: "ha".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            &conn,
            a,
            &[Symbol {
                name: "alpha".into(),
                kind: SymbolKind::Function,
                file: PathBuf::from("pkg/a.py"),
                start_line: 1,
                start_col: 1,
                module_path: "pkg.a".into(),
                container: String::new(),
            }],
        )
        .unwrap();
        let main = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("main.py"),
                content_hash: "hm".into(),
            },
        )
        .unwrap();
        queries::insert_references(
            &conn,
            main,
            &[Reference {
                name: "alpha".into(),
                file: PathBuf::from("main.py"),
                start_line: 3,
                start_col: 7,
                kind: ReferenceKind::Call,
                container: "main.py".into(),
                qualifier: String::new(),
            }],
        )
        .unwrap();

        let defs = queries::find_definition(&conn, "alpha").unwrap();
        let (impact, file_hits) = find_impact_from_defs_with_file_hits(&conn, &defs).unwrap();
        assert!(impact.is_empty(), "expected no symbols, got {impact:?}");
        assert_eq!(file_hits, vec!["main.py".to_string()]);
    }

    #[test]
    fn find_impact_terminates_on_cycle() {
        let conn = setup();
        fixture_cycle(&conn);

        let impact = find_impact(&conn, "x").unwrap();
        let names: Vec<&str> = impact.iter().map(|s| s.name.as_str()).collect();
        // Transitive callers of x are y and z; the z→x and y→x edges must not
        // re-expand forever.
        assert_eq!(names, vec!["y", "z"]);
        // Idempotent under re-query (no growth from residual cycle state).
        let again = find_impact(&conn, "x").unwrap();
        assert_eq!(again.len(), impact.len());
    }

    #[test]
    fn find_impact_from_defs_seeds_only_given_identity() {
        let conn = setup();
        // Two `serve` symbols in different modules; only mcp::serve is referenced.
        let api = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("src/api.rs"),
                content_hash: "a".into(),
            },
        )
        .unwrap();
        let mcp = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("src/mcp.rs"),
                content_hash: "m".into(),
            },
        )
        .unwrap();
        let main = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("src/main.rs"),
                content_hash: "main".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            &conn,
            api,
            &[Symbol {
                name: "serve".into(),
                kind: SymbolKind::Function,
                file: PathBuf::from("src/api.rs"),
                start_line: 1,
                start_col: 1,
                module_path: "crate::api".into(),
                container: String::new(),
            }],
        )
        .unwrap();
        queries::insert_symbols(
            &conn,
            mcp,
            &[Symbol {
                name: "serve".into(),
                kind: SymbolKind::Function,
                file: PathBuf::from("src/mcp.rs"),
                start_line: 1,
                start_col: 1,
                module_path: "crate::mcp".into(),
                container: String::new(),
            }],
        )
        .unwrap();
        queries::insert_symbols(
            &conn,
            main,
            &[Symbol {
                name: "boot".into(),
                kind: SymbolKind::Function,
                file: PathBuf::from("src/main.rs"),
                start_line: 1,
                start_col: 1,
                module_path: "crate".into(),
                container: String::new(),
            }],
        )
        .unwrap();
        queries::insert_imports(
            &conn,
            main,
            &[crate::graph::types::Import {
                module_path: "crate::mcp::serve".into(),
                alias: None,
                file: PathBuf::from("src/main.rs"),
            }],
        )
        .unwrap();
        queries::insert_references(
            &conn,
            main,
            &[Reference {
                name: "serve".into(),
                file: PathBuf::from("src/main.rs"),
                start_line: 2,
                start_col: 10,
                kind: ReferenceKind::Call,
                container: "crate::boot".into(),
                qualifier: String::new(),
            }],
        )
        .unwrap();

        let mcp_serve = queries::find_definition_by_qualified(&conn, "crate::mcp", "serve").unwrap();
        assert_eq!(mcp_serve.len(), 1);
        let impact = find_impact_from_defs(&conn, &mcp_serve).unwrap();
        let names: Vec<&str> = impact.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["boot"]);

        // Seeding only api::serve (unreferenced) must not pull boot via bare name.
        let api_serve = queries::find_definition_by_qualified(&conn, "crate::api", "serve").unwrap();
        let impact_api = find_impact_from_defs(&conn, &api_serve).unwrap();
        assert!(
            impact_api.is_empty(),
            "api::serve has no callers; got {impact_api:?}"
        );
    }

    #[test]
    fn tier3_method_call_does_not_match_unrelated_free_function() {
        let conn = setup();
        // Free function `get` in one module, used via a method call in the
        // same file (tier 2, kept) and via `client.get()` from another
        // module with no import (tier 3, dropped).
        let fa = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("src/a.py"),
                content_hash: "ha".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            &conn,
            fa,
            &[
                Symbol {
                    name: "get".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: 1,
                    start_col: 1,
                    module_path: "a".into(),
                    container: String::new(),
                },
                Symbol {
                    name: "local_user".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: 5,
                    start_col: 1,
                    module_path: "a".into(),
                    container: String::new(),
                },
            ],
        )
        .unwrap();
        queries::insert_references(
            &conn,
            fa,
            &[Reference {
                name: "get".into(),
                file: PathBuf::new(),
                start_line: 6,
                start_col: 5,
                kind: ReferenceKind::Method,
                container: "a::local_user".into(),
                qualifier: String::new(),
            }],
        )
        .unwrap();
        let fb = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("src/b.py"),
                content_hash: "hb".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            &conn,
            fb,
            &[Symbol {
                name: "use_client".into(),
                kind: SymbolKind::Function,
                file: PathBuf::new(),
                start_line: 1,
                start_col: 1,
                module_path: "b".into(),
                container: String::new(),
            }],
        )
        .unwrap();
        queries::insert_references(
            &conn,
            fb,
            &[Reference {
                name: "get".into(),
                file: PathBuf::new(),
                start_line: 2,
                start_col: 5,
                kind: ReferenceKind::Method,
                container: "b::use_client".into(),
                qualifier: String::new(),
            }],
        )
        .unwrap();

        let impact = find_impact(&conn, "get").unwrap();
        let names: Vec<&str> = impact.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["local_user"]);
    }

    /// Two-file fixture: `helper` defined only in m.ts; a.ts binds the
    /// module under `alias` and calls `<receiver>.helper()` from `run`.
    fn fixture_namespace_call(conn: &Connection, alias: Option<&str>, receiver: &str) {
        let m = queries::insert_file(
            conn,
            &FileNode {
                path: PathBuf::from("m.ts"),
                content_hash: "hm".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            conn,
            m,
            &[Symbol {
                name: "helper".into(),
                kind: SymbolKind::Function,
                file: PathBuf::new(),
                start_line: 1,
                start_col: 1,
                module_path: "m".into(),
                container: String::new(),
            }],
        )
        .unwrap();
        let a = queries::insert_file(
            conn,
            &FileNode {
                path: PathBuf::from("a.ts"),
                content_hash: "ha".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            conn,
            a,
            &[Symbol {
                name: "run".into(),
                kind: SymbolKind::Function,
                file: PathBuf::new(),
                start_line: 5,
                start_col: 1,
                module_path: "a".into(),
                container: String::new(),
            }],
        )
        .unwrap();
        queries::insert_imports(
            conn,
            a,
            &[Import {
                module_path: "m".into(),
                alias: alias.map(str::to_string),
                file: PathBuf::new(),
            }],
        )
        .unwrap();
        queries::insert_references(
            conn,
            a,
            &[Reference {
                name: "helper".into(),
                file: PathBuf::new(),
                start_line: 6,
                start_col: 5,
                kind: ReferenceKind::Method,
                container: "a::run".into(),
                qualifier: receiver.into(),
            }],
        )
        .unwrap();
    }

    #[test]
    fn namespace_qualified_singleton_method_call_attributes_via_import_alias() {
        let conn = setup();
        // `import * as ns from "./m"` + `ns.helper()`: the only `helper`
        // def, reached through a namespace alias the tier computation
        // can't see (tier 3). The qualifier resolving via the import row
        // is precise evidence — impact must include `run`.
        fixture_namespace_call(&conn, Some("ns"), "ns");

        let impact = find_impact(&conn, "helper").unwrap();
        let names: Vec<&str> = impact.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["run"]);
    }

    #[test]
    fn singleton_method_call_with_unrelated_import_stays_dropped() {
        let conn = setup();
        // `client.helper()` where a.ts imports m under an unrelated alias:
        // the module is imported but the receiver is unknown, so the
        // singleton must still fall to the tier-3 method gate (dropped).
        // Passing here also proves the companion test ranks tier 3 (a
        // tier ≤ 2 singleton would accept via plain identity match).
        fixture_namespace_call(&conn, Some("other"), "client");

        let impact = find_impact(&conn, "helper").unwrap();
        assert!(impact.is_empty(), "expected empty, got {impact:?}");
    }
}
