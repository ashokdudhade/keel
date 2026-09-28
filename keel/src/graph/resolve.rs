//! Deterministic, module/import-aware definition resolution.
//!
//! Ranking tiers (lower is better):
//! 1. Exact `module_path::name` reachable via an `imports` row in the caller's file
//! 2. Same-module match (`from` is a module path, or a file whose symbols share that module)
//! 3. All name matches (v0.1 fallback)
//!
//! Within a tier, results are ordered by `(path, line, col)`.

use crate::db::queries;
use crate::error::Result;
use crate::graph::types::{Reference, ReferenceKind, Symbol};
use rusqlite::Connection;
use std::path::Path;

/// Resolve definitions of `name` relative to `from_file_or_module`.
///
/// `from_file_or_module` may be a file path (for import-aware ranking) and/or a
/// module path (for same-module ranking). All name matches are returned,
/// ordered by tier then `(path, line, col)`.
pub fn resolve_definition(
    conn: &Connection,
    name: &str,
    from_file_or_module: &str,
) -> Result<Vec<Symbol>> {
    Ok(resolve_definition_ranked(conn, name, from_file_or_module)?
        .into_iter()
        .map(|(_, s)| s)
        .collect())
}

/// Like [`resolve_definition`], but each symbol is paired with its ranking tier
/// (1 = exact import, 2 = last-segment import fallback or same-module,
/// 3 = name-only fallback).
pub fn resolve_definition_ranked(
    conn: &Connection,
    name: &str,
    from_file_or_module: &str,
) -> Result<Vec<(u8, Symbol)>> {
    let candidates = queries::find_definition(conn, name)?;
    if candidates.is_empty() {
        return Ok(Vec::new());
    }

    let imports = queries::imports_for_file(conn, from_file_or_module)?;
    let file_modules = queries::module_paths_in_file(conn, from_file_or_module)?;

    let mut ranked: Vec<(u8, Symbol)> = candidates
        .into_iter()
        .map(|sym| {
            let tier = rank_tier(&sym, name, from_file_or_module, &imports, &file_modules);
            (tier, sym)
        })
        .collect();

    ranked.sort_by(|(ta, a), (tb, b)| {
        ta.cmp(tb)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.start_line.cmp(&b.start_line))
            .then_with(|| a.start_col.cmp(&b.start_col))
    });

    Ok(ranked)
}

/// Accept a top match for dependency / impact edges only when it is unique or
/// ranked at tier ≤ 2 (never rely on tier-3 path order alone).
pub fn acceptable_top_match(ranked: &[(u8, Symbol)]) -> Option<&Symbol> {
    match ranked {
        [] => None,
        [(_, sym)] => Some(sym),
        [(tier, sym), ..] if *tier <= 2 => Some(sym),
        _ => None,
    }
}

/// Find call sites of `name`.
///
/// When `target_module` is `Some`, keep only references that resolve to that
/// module. If multiple same-named definitions exist, the resolution must be
/// precise (tier 1 import or tier 2 same-module) so a bare name-match does not
/// attribute callers to the path-ordered fallback. When `None`, returns all
/// name-matched references in stable order (v0.1 behavior).
pub fn find_callers(
    conn: &Connection,
    name: &str,
    target_module: Option<&str>,
) -> Result<Vec<Reference>> {
    let refs = queries::find_references(conn, name)?;
    let Some(target_module) = target_module else {
        return Ok(refs);
    };

    let def_count = queries::find_definition(conn, name)?.len();
    let require_precise = def_count > 1;

    let mut out = Vec::new();
    for r in refs {
        let from = r.file.to_string_lossy();
        let from = from.as_ref();
        let imports = queries::imports_for_file(conn, from)?;
        let file_modules = queries::module_paths_in_file(conn, from)?;
        let ranked = resolve_definition_ranked(conn, name, from)?;
        let Some((top_tier, top)) = ranked.first() else {
            continue;
        };
        if require_precise
            && matches!(r.kind, ReferenceKind::Path | ReferenceKind::Method)
            && !r.qualifier.is_empty()
        {
            // The stored qualifier (`mcp` in `mcp::serve`, `db` in `db.get()`)
            // breaks import-tier ties: attribute only when it uniquely selects
            // the target among the tied top-rank candidates. Otherwise fall
            // through to the conservative handling below (ties stay dropped).
            // Below tier 2 (e.g. namespace imports, which the tier computation
            // can't see) the qualifier must resolve through an import row —
            // the last-segment fallback alone is too weak there.
            let strict = *top_tier > 2;
            let matches = |module: &str| {
                if strict {
                    qualifier_matches_via_import(&r.qualifier, module, &imports)
                } else {
                    qualifier_matches_module(&r.qualifier, module, &imports)
                }
            };
            let tied: Vec<&(u8, Symbol)> = ranked
                .iter()
                .take_while(|(tier, _)| *tier == *top_tier)
                .collect();
            let selects_target = matches(target_module);
            let selects_other = tied
                .iter()
                .any(|(_, sym)| sym.module_path != target_module && matches(&sym.module_path));
            let target_tied = tied
                .iter()
                .any(|(_, sym)| sym.module_path == target_module);
            if selects_target && !selects_other && target_tied {
                out.push(r);
                continue;
            }
        }
        // Member reads (`c.port`) and method calls on locals
        // (`c.describe()`) name the module only through the receiver's
        // type, which the index can't see: the import binds `Config`,
        // not `port`/`describe`. When nothing resolves precisely and
        // the target module is imported by the use file (under any
        // alias), attribute there.
        if require_precise
            && matches!(r.kind, ReferenceKind::Value | ReferenceKind::Method)
            && !r.qualifier.is_empty()
            && *top_tier > 2
            && ranked.iter().any(|(_, s)| s.module_path == target_module)
            && imports
                .iter()
                .any(|(p, _)| import_row_matches_module(p, target_module))
            // A qualifier resolving through an import row to another
            // ranked module is precise evidence (`b.serve()` belongs to
            // `b`): it beats the import fallback.
            && !ranked.iter().any(|(_, s)| {
                s.module_path != target_module
                    && qualifier_matches_via_import(&r.qualifier, &s.module_path, &imports)
            })
        {
            out.push(r);
            continue;
        }
        if top.module_path != target_module {
            continue;
        }
        if require_precise {
            let tier = rank_tier(top, name, from, &imports, &file_modules);
            if tier > 2 {
                continue;
            }
            // Qualified-syntax refs (Path/Method) on a module tie are
            // unattributable without a deciding qualifier: drop rather than
            // report High on the deterministic-first winner.
            let tied = ranked
                .get(1)
                .is_some_and(|(tier, sym)| *tier == *top_tier && sym.module_path != top.module_path);
            if tied && matches!(r.kind, ReferenceKind::Path | ReferenceKind::Method) {
                continue;
            }
        }
        out.push(r);
    }
    Ok(out)
}

/// Find uses of `bare` through import aliases (`serveA()` for `a::serve`).
///
/// Each hit names the exact module the alias maps to, so attribution stays
/// precise even when several modules define `bare`. When `target_module` is
/// set, only aliases mapping into it are returned. Aliases shadowed by a
/// same-file definition of the alias name are skipped (the local wins).
/// Empty when `bare` has no indexed definitions.
pub fn find_alias_uses(
    conn: &Connection,
    bare: &str,
    target_module: Option<&str>,
) -> Result<Vec<Reference>> {
    let defs = queries::find_definition(conn, bare)?;
    if defs.is_empty() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for (file, module_path, alias) in queries::aliased_imports(conn)? {
        if last_segment(&module_path) != Some(bare) {
            continue;
        }
        let from_module = module_path_prefix(&module_path);
        let maps_to_target = match target_module {
            Some(t) => import_matches_module(from_module, t),
            None => defs
                .iter()
                .any(|d| import_matches_module(from_module, &d.module_path)),
        };
        if !maps_to_target {
            continue;
        }
        // A same-file definition of the alias shadows the import (Python).
        let shadowed = queries::find_definition(conn, &alias)?
            .iter()
            .any(|d| d.file.to_string_lossy() == file);
        if shadowed {
            continue;
        }
        for reference in queries::find_references(conn, &alias)? {
            if reference.file.to_string_lossy() == file {
                out.push(reference);
            }
        }
    }
    out.sort_by(|a, b| {
        (a.file.clone(), a.start_line, a.start_col)
            .cmp(&(b.file.clone(), b.start_line, b.start_col))
    });
    Ok(out)
}

/// True when a stored qualifier identifies `module` from the caller's file.
///
/// Matches (in order): the qualifier naming the module outright (absolute
/// paths like `crate::mcp`, dotted paths like `a.b`), an aliased import
/// (`use x as q`), a plain import whose last segment equals the qualifier's
/// last segment, and — with no import evidence — a relative path whose last
/// segment equals the module's last segment. Separators are `::` (Rust) and
/// `.` (Python/TypeScript/JavaScript/Go) interchangeably.
pub(crate) fn qualifier_matches_module(
    qualifier: &str,
    module: &str,
    imports: &[(String, Option<String>)],
) -> bool {
    qualifier_matches_inner(qualifier, module, imports, false)
}

/// Final path segment across `::`, `.`, and `/` separators.
fn last_segment(path: &str) -> Option<&str> {
    path.rsplit([':', '.', '/']).find(|s| !s.is_empty())
}

/// Strict qualifier match: like [`qualifier_matches_module`] but without the
/// last-segment fallback — the qualifier must name the module outright (an
/// absolute path) or resolve through an import row. Bare equality alone is
/// coincidence, not evidence. Used when no import tier was reached, where
/// the fallback would be the only evidence.
pub(crate) fn qualifier_matches_via_import(
    qualifier: &str,
    module: &str,
    imports: &[(String, Option<String>)],
) -> bool {
    qualifier_matches_inner(qualifier, module, imports, true)
}

fn qualifier_matches_inner(
    qualifier: &str,
    module: &str,
    imports: &[(String, Option<String>)],
    strict: bool,
) -> bool {
    let qualifier = qualifier.trim_start_matches(':');
    if qualifier.is_empty() || module.is_empty() {
        return false;
    }
    let Some(q_last) = last_segment(qualifier) else {
        return false;
    };
    if qualifier == module {
        // Absolute paths need no import row in either mode. Bare
        // equality in strict mode is coincidence (`c.describe()` vs
        // module `c`) unless an import row mentions the qualifier.
        if !strict || qualifier_is_path_like(qualifier) {
            return true;
        }
        let mentioned = imports.iter().any(|(path, alias)| {
            alias.as_deref() == Some(q_last) || last_segment(path).unwrap_or("") == q_last
        });
        if mentioned {
            return true;
        }
    }
    for (path, alias) in imports {
        if let Some(alias) = alias {
            if alias == q_last && import_matches_module(path, module) {
                return true;
            }
        } else {
            let local = last_segment(path).unwrap_or("");
            if local == q_last
                && (import_matches_module(path, module)
                    || import_matches_module(module_path_prefix(path), module))
            {
                return true;
            }
        }
    }
    if strict {
        return false;
    }
    last_segment(module) == Some(q_last)
}

/// True when a qualifier is shaped like a module path rather than a bare
/// name (`crate::mcp`, `a/b`, `a.b`).
fn qualifier_is_path_like(qualifier: &str) -> bool {
    qualifier.contains("::") || qualifier.contains('/') || qualifier.contains('.')
}

fn rank_tier(
    sym: &Symbol,
    name: &str,
    from_file_or_module: &str,
    imports: &[(String, Option<String>)],
    file_modules: &[String],
) -> u8 {
    if let Some(tier) = import_reach(sym, name, imports) {
        return tier;
    }
    if same_module(sym, from_file_or_module, file_modules) {
        return 2;
    }
    3
}

/// Import tier at which `sym` is reachable: 1 for an exact import (path
/// equals `module_path::name`, or the alias equals the lookup name while
/// the path points at that symbol or module), 2 for a last-segment
/// fallback match, `None` when no import reaches it.
///
/// Exact and fallback must stay distinct tiers: a last-segment match is
/// cross-scheme evidence (`ts/src/h` ends in `h`, but so does Go package
/// `h`), so it must never tie with — let alone outrank by path order —
/// the exact import.
fn import_reach(
    sym: &Symbol,
    name: &str,
    imports: &[(String, Option<String>)],
) -> Option<u8> {
    let qualified = qualified_name(&sym.module_path, &sym.name);
    let mut best: Option<u8> = None;
    let mut consider = |tier: Option<u8>| {
        best = match (best, tier) {
            (Some(b), Some(t)) => Some(b.min(t)),
            (b, t) => b.or(t),
        };
    };
    for (module_path, alias) in imports {
        // Go dot-imports (alias `.`) import every name bare, so the use
        // name must equal the definition name, exactly like an unaliased
        // import.
        let aliased = match alias {
            Some(a) if a == "." => sym.name == name,
            Some(a) => a == name,
            None => true,
        };
        if !aliased {
            continue;
        }
        if module_path == &qualified {
            consider(Some(1));
        }
        if sym.name == name {
            consider(module_match_tier(module_path, &sym.module_path));
        }
        // Aliased imports pin the lookup name, so the path suffix must be
        // the definition name; unaliased ones match the lookup name.
        let end_name = if alias.is_some() { &sym.name } else { name };
        if path_ends_with_name(module_path, end_name) {
            consider(module_match_tier(
                module_path_prefix(module_path),
                &sym.module_path,
            ));
        }
    }
    best
}

/// Tier at which an import path identifies `module_path`: 1 for exact,
/// 2 for Go-style last path segment / trailing `::` segment, `None` for
/// no match.
fn module_match_tier(import_path: &str, module_path: &str) -> Option<u8> {
    if import_path == module_path {
        return Some(1);
    }
    if import_path.rsplit('/').next() == Some(module_path) {
        return Some(2);
    }
    if import_path.rsplit("::").next() == Some(module_path) {
        return Some(2);
    }
    None
}

/// True when an import path identifies `module_path` (exact, or Go-style last
/// path segment / trailing `::` segment).
pub(crate) fn import_matches_module(import_path: &str, module_path: &str) -> bool {
    module_match_tier(import_path, module_path).is_some()
}

/// True when an import row identifies `module`: the row may carry a
/// `::name` suffix (`ts/src/j::Config`), so the prefix counts too.
pub(crate) fn import_row_matches_module(import_path: &str, module: &str) -> bool {
    import_matches_module(import_path, module)
        || import_matches_module(module_path_prefix(import_path), module)
}

fn same_module(sym: &Symbol, from_file_or_module: &str, file_modules: &[String]) -> bool {
    if sym.module_path == from_file_or_module {
        return true;
    }
    // When `from` is a file path, treat modules defined in that file as local.
    if Path::new(from_file_or_module).extension().is_some()
        || from_file_or_module.contains('/')
        || from_file_or_module.contains('\\')
    {
        return file_modules.iter().any(|m| m == &sym.module_path);
    }
    false
}

fn qualified_name(module_path: &str, name: &str) -> String {
    if module_path.is_empty() {
        name.to_string()
    } else {
        format!("{module_path}::{name}")
    }
}

fn path_ends_with_name(module_path: &str, name: &str) -> bool {
    module_path
        .rsplit("::")
        .next()
        .is_some_and(|seg| seg == name)
}

fn module_path_prefix(module_path: &str) -> &str {
    match module_path.rfind("::") {
        Some(i) => &module_path[..i],
        None => "",
    }
}

/// Suggest up to `limit` indexed symbol names near `name` for miss recovery.
///
/// Ranking: case-insensitive exact match, then case-insensitive substring,
/// then Levenshtein within a length-scaled threshold; ties break by name so
/// output is deterministic. Returns empty for blank or single-char input.
/// Runs on the miss path only (empty results), never on hits.
pub fn suggest_names(
    conn: &Connection,
    name: &str,
    limit: usize,
) -> Result<Vec<String>> {
    let candidates = queries::distinct_symbol_names(conn)?;
    Ok(rank_candidates(name, &candidates, limit))
}

/// Pure ranking behind [`suggest_names`]: score each candidate, sort, take.
/// Also ranks file paths for outline miss recovery.
pub(crate) fn rank_candidates(query: &str, candidates: &[String], limit: usize) -> Vec<String> {
    if limit == 0 || query.chars().count() < 2 {
        return Vec::new();
    }
    let query_lower = query.to_lowercase();
    let query_len = query.chars().count();
    // Allow roughly one typo per 3 chars, clamped so short queries stay strict
    // and long ones don't match the whole index.
    let max_distance = (query_len / 3).clamp(1, 3);
    let mut scored: Vec<(u8, usize, &str)> = Vec::new();
    for candidate in candidates {
        let lower = candidate.to_lowercase();
        let cand_len = candidate.chars().count();
        if lower == query_lower {
            scored.push((0, 0, candidate));
        } else if lower.contains(&query_lower)
            // Reverse direction (query contains the name) only when the name
            // covers at least half the query: otherwise any short symbol
            // inside a long nonsense string ("Symbol" in
            // "NonexistentSymbolXYZ123") suggests noise.
            || (cand_len >= 4 && cand_len * 2 >= query_len && query_lower.contains(&lower))
        {
            scored.push((1, 0, candidate));
        } else {
            let distance = levenshtein(&query_lower, &lower);
            if distance <= max_distance {
                scored.push((2, distance, candidate));
            }
        }
    }
    scored.sort_by(|a, b| (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, _, name)| name.to_string())
        .collect()
}

/// Edit distance over chars (two-row DP; queries are short).
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut curr = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        curr[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            curr[j + 1] = (prev[j + 1] + 1).min(curr[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{queries, schema};
    use crate::graph::types::{
        FileNode, Import, Reference, ReferenceKind, Symbol, SymbolKind,
    };
    use rusqlite::Connection;
    use std::path::PathBuf;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        schema::initialize(&conn).expect("init schema");
        conn
    }

    /// Two same-named helpers in different modules, plus an importer file.
    fn fixture_two_helpers(conn: &Connection) {
        let a = queries::insert_file(
            conn,
            &FileNode {
                path: PathBuf::from("src/a.rs"),
                content_hash: "ha".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            conn,
            a,
            &[Symbol {
                name: "helper".into(),
                kind: SymbolKind::Function,
                file: PathBuf::new(),
                start_line: 2,
                start_col: 1,
                module_path: "crate::a".into(),
            }],
        )
        .unwrap();

        let b = queries::insert_file(
            conn,
            &FileNode {
                path: PathBuf::from("src/b.rs"),
                content_hash: "hb".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            conn,
            b,
            &[Symbol {
                name: "helper".into(),
                kind: SymbolKind::Function,
                file: PathBuf::new(),
                start_line: 2,
                start_col: 1,
                module_path: "crate::b".into(),
            }],
        )
        .unwrap();

        let c = queries::insert_file(
            conn,
            &FileNode {
                path: PathBuf::from("src/c.rs"),
                content_hash: "hc".into(),
            },
        )
        .unwrap();
        queries::insert_imports(
            conn,
            c,
            &[Import {
                module_path: "crate::a::helper".into(),
                alias: None,
                file: PathBuf::new(),
            }],
        )
        .unwrap();
        queries::insert_references(
            conn,
            c,
            &[Reference {
                name: "helper".into(),
                file: PathBuf::new(),
                start_line: 10,
                start_col: 5,
                kind: ReferenceKind::Call,
                container: "main".into(),
                qualifier: String::new(),
            }],
        )
        .unwrap();

        let d = queries::insert_file(
            conn,
            &FileNode {
                path: PathBuf::from("src/d.rs"),
                content_hash: "hd".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            conn,
            d,
            &[Symbol {
                name: "other".into(),
                kind: SymbolKind::Function,
                file: PathBuf::new(),
                start_line: 1,
                start_col: 1,
                module_path: "crate::d".into(),
            }],
        )
        .unwrap();
        queries::insert_references(
            conn,
            d,
            &[Reference {
                name: "helper".into(),
                file: PathBuf::new(),
                start_line: 4,
                start_col: 5,
                kind: ReferenceKind::Call,
                container: "other".into(),
                qualifier: String::new(),
            }],
        )
        .unwrap();
    }

    #[test]
    fn import_aware_resolution_ranks_imported_symbol_first() {
        let conn = setup();
        fixture_two_helpers(&conn);

        let ranked = resolve_definition(&conn, "helper", "src/c.rs").unwrap();
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].module_path, "crate::a");
        assert_eq!(ranked[0].file, PathBuf::from("src/a.rs"));
        assert_eq!(ranked[1].module_path, "crate::b");
    }

    /// Regression: `use crate::b::helper as helper` must not give tier 1 to
    /// every same-named symbol (path order would then prefer crate::a).
    #[test]
    fn aliased_import_requires_module_prefix_match() {
        let conn = setup();
        fixture_two_helpers(&conn);

        let e = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("src/e.rs"),
                content_hash: "he".into(),
            },
        )
        .unwrap();
        queries::insert_imports(
            &conn,
            e,
            &[Import {
                module_path: "crate::b::helper".into(),
                alias: Some("helper".into()),
                file: PathBuf::new(),
            }],
        )
        .unwrap();

        let ranked = resolve_definition(&conn, "helper", "src/e.rs").unwrap();
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].module_path, "crate::b");
        assert_eq!(ranked[0].file, PathBuf::from("src/b.rs"));
        assert_eq!(ranked[1].module_path, "crate::a");
    }

    #[test]
    fn same_module_wins_without_import() {
        let conn = setup();
        fixture_two_helpers(&conn);

        let ranked = resolve_definition(&conn, "helper", "crate::b").unwrap();
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].module_path, "crate::b");
        assert_eq!(ranked[0].file, PathBuf::from("src/b.rs"));
    }

    #[test]
    fn neither_import_nor_same_module_returns_all_stable() {
        let conn = setup();
        fixture_two_helpers(&conn);

        let ranked = resolve_definition(&conn, "helper", "crate::d").unwrap();
        assert_eq!(ranked.len(), 2);
        // Stable v0.1 order: path, then line, then col.
        assert_eq!(ranked[0].file, PathBuf::from("src/a.rs"));
        assert_eq!(ranked[1].file, PathBuf::from("src/b.rs"));
    }

    #[test]
    fn find_definition_by_qualified_matches_module_and_name() {
        let conn = setup();
        fixture_two_helpers(&conn);

        let found = queries::find_definition_by_qualified(&conn, "crate::a", "helper").unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].module_path, "crate::a");
        assert_eq!(found[0].file, PathBuf::from("src/a.rs"));

        let missing = queries::find_definition_by_qualified(&conn, "crate::z", "helper").unwrap();
        assert!(missing.is_empty());
    }

    #[test]
    fn find_callers_filters_by_resolved_target_module() {
        let conn = setup();
        fixture_two_helpers(&conn);

        // Without a target: all name-matched call sites (c.rs + d.rs).
        let all = find_callers(&conn, "helper", None).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].file, PathBuf::from("src/c.rs"));
        assert_eq!(all[1].file, PathBuf::from("src/d.rs"));

        // With target crate::a: only the importer in c.rs resolves to it.
        let precise = find_callers(&conn, "helper", Some("crate::a")).unwrap();
        assert_eq!(precise.len(), 1);
        assert_eq!(precise[0].file, PathBuf::from("src/c.rs"));
    }

    /// Two same-named constants where the last-segment fallback would
    /// collide: TS module `ts/src/h` and Go package `h`.
    fn fixture_limit_collision(conn: &Connection) {
        let file = |path: &str| {
            queries::insert_file(
                conn,
                &FileNode {
                    path: PathBuf::from(path),
                    content_hash: path.into(),
                },
            )
            .unwrap()
        };
        let def = |module: &str| Symbol {
            name: "LIMIT".into(),
            kind: SymbolKind::Const,
            file: PathBuf::new(),
            start_line: 1,
            start_col: 1,
            module_path: module.into(),
        };
        // The Go file sorts first, so a tier tie would resolve to it.
        queries::insert_symbols(conn, file("go/h.go"), &[def("h")]).unwrap();
        queries::insert_symbols(conn, file("ts/src/h.ts"), &[def("ts/src/h")]).unwrap();
        let ts_i = file("ts/src/i.ts");
        queries::insert_imports(
            conn,
            ts_i,
            &[Import {
                module_path: "ts/src/h".into(),
                alias: None,
                file: PathBuf::new(),
            }],
        )
        .unwrap();
        let go_i = file("go/i.go");
        queries::insert_imports(
            conn,
            go_i,
            &[Import {
                module_path: "example.com/impact/h".into(),
                alias: Some(".".into()),
                file: PathBuf::new(),
            }],
        )
        .unwrap();
    }

    #[test]
    fn last_segment_imports_rank_below_exact_ones() {
        let conn = setup();
        fixture_limit_collision(&conn);

        // From the TS importer the exact import wins outright, even
        // though `ts/src/h` also ends in `h`.
        let ranked = resolve_definition_ranked(&conn, "LIMIT", "ts/src/i.ts").unwrap();
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].1.module_path, "ts/src/h");
        assert_eq!(ranked[0].0, 1);
        assert_eq!(ranked[1].1.module_path, "h");
        assert_eq!(ranked[1].0, 2);
    }

    #[test]
    fn member_reads_attribute_via_imported_module() {
        let conn = setup();
        let file = |path: &str| {
            queries::insert_file(
                &conn,
                &FileNode {
                    path: PathBuf::from(path),
                    content_hash: path.into(),
                },
            )
            .unwrap()
        };
        let def = |module: &str| Symbol {
            name: "port".into(),
            kind: SymbolKind::Other("field".into()),
            file: PathBuf::new(),
            start_line: 2,
            start_col: 3,
            module_path: module.into(),
        };
        queries::insert_symbols(&conn, file("js/src/j.js"), &[def("js/src/j")]).unwrap();
        queries::insert_symbols(&conn, file("ts/src/j.ts"), &[def("ts/src/j")]).unwrap();
        // The use file imports the class, not the field: the row carries a
        // `::Config` suffix the member rule must see through.
        let use_file = file("ts/src/k.ts");
        queries::insert_imports(
            &conn,
            use_file,
            &[Import {
                module_path: "ts/src/j::Config".into(),
                alias: None,
                file: PathBuf::new(),
            }],
        )
        .unwrap();
        queries::insert_references(
            &conn,
            use_file,
            &[Reference {
                name: "port".into(),
                file: PathBuf::new(),
                start_line: 4,
                start_col: 12,
                kind: ReferenceKind::Value,
                container: "ts/src/k::getPort".into(),
                qualifier: "c".into(),
            }],
        )
        .unwrap();

        // The imported module wins despite the row naming the class.
        let refs = find_callers(&conn, "port", Some("ts/src/j")).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].file, PathBuf::from("ts/src/k.ts"));
        // The other module's identical field stays unattributed here.
        let refs = find_callers(&conn, "port", Some("js/src/j")).unwrap();
        assert!(refs.is_empty());
    }

    #[test]
    fn method_calls_on_locals_attribute_via_imported_module() {
        let conn = setup();
        let file = |path: &str| {
            queries::insert_file(
                &conn,
                &FileNode {
                    path: PathBuf::from(path),
                    content_hash: path.into(),
                },
            )
            .unwrap()
        };
        let def = |module: &str| Symbol {
            name: "describe".into(),
            kind: SymbolKind::Function,
            file: PathBuf::new(),
            start_line: 5,
            start_col: 9,
            module_path: module.into(),
        };
        queries::insert_symbols(&conn, file("a.py"), &[def("a")]).unwrap();
        queries::insert_symbols(&conn, file("c.py"), &[def("c")]).unwrap();
        // `c.describe()` in a file importing module `a`: the qualifier is
        // a local that merely shares a name with module `c`.
        let use_file = file("b.py");
        queries::insert_imports(
            &conn,
            use_file,
            &[Import {
                module_path: "a::Config".into(),
                alias: None,
                file: PathBuf::new(),
            }],
        )
        .unwrap();
        queries::insert_references(
            &conn,
            use_file,
            &[Reference {
                name: "describe".into(),
                file: PathBuf::new(),
                start_line: 5,
                start_col: 14,
                kind: ReferenceKind::Method,
                container: "b::run".into(),
                qualifier: "c".into(),
            }],
        )
        .unwrap();

        // The imported module wins; the same-named module stays out.
        let refs = find_callers(&conn, "describe", Some("a")).unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].file, PathBuf::from("b.py"));
        let refs = find_callers(&conn, "describe", Some("c")).unwrap();
        assert!(refs.is_empty());
    }

    #[test]
    fn go_dot_imports_reach_bare_names() {
        let conn = setup();
        fixture_limit_collision(&conn);

        // From the dot-importing Go file the bare package resolves at
        // the fallback tier while the unrelated TS module stays at the
        // name-only tier.
        let ranked = resolve_definition_ranked(&conn, "LIMIT", "go/i.go").unwrap();
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].1.module_path, "h");
        assert_eq!(ranked[0].0, 2);
        assert_eq!(ranked[1].1.module_path, "ts/src/h");
        assert_eq!(ranked[1].0, 3);
    }

    #[test]
    fn strict_qualifier_match_requires_import_evidence() {
        let imports = vec![("b".to_string(), Some("b".to_string()))];
        // Namespace import row: strict matches.
        assert!(qualifier_matches_via_import("b", "b", &imports));
        // No import row mentions module `x::a`: strict refuses the tail
        // fallback...
        assert!(!qualifier_matches_via_import("a", "x::a", &imports));
        // ...while the lenient matcher still takes it.
        assert!(qualifier_matches_module("a", "x::a", &imports));
        // Absolute paths need no import row in either mode.
        assert!(qualifier_matches_via_import("crate::mcp", "crate::mcp", &[]));
        assert!(!qualifier_matches_via_import("mcp", "crate::api", &imports));
    }

    #[test]
    fn strict_exact_qualifiers_need_import_evidence() {
        // A bare qualifier equal to the module with no mentioning row is
        // coincidence (`c.describe()` vs module `c`): strict refuses it.
        let unrelated = vec![("a::Config".to_string(), None)];
        assert!(!qualifier_matches_via_import("c", "c", &unrelated));
        // A mentioning row (alias or path tail) restores the match.
        let aliased = vec![("store".to_string(), Some("db".to_string()))];
        assert!(qualifier_matches_via_import("db", "db", &aliased));
        let pathed = vec![("example.com/app/helper".to_string(), None)];
        assert!(qualifier_matches_via_import("helper", "helper", &pathed));
        // Lenient matching keeps bare equality (other evidence exists).
        assert!(qualifier_matches_module("c", "c", &unrelated));
    }

    #[test]
    fn find_callers_drops_path_refs_on_module_ties() {
        let conn = setup();
        for (file, module) in [("src/a.rs", "crate::a"), ("src/b.rs", "crate::b")] {
            let id = queries::insert_file(
                &conn,
                &FileNode {
                    path: PathBuf::from(file),
                    content_hash: "h".into(),
                },
            )
            .unwrap();
            queries::insert_symbols(
                &conn,
                id,
                &[Symbol {
                    name: "serve".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: 1,
                    start_col: 1,
                    module_path: module.into(),
                }],
            )
            .unwrap();
        }
        // c.rs imports BOTH modules: a qualified `serve` site there ties at
        // tier 1, and the dropped qualifier makes it unattributable.
        let c = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("src/c.rs"),
                content_hash: "hc".into(),
            },
        )
        .unwrap();
        queries::insert_imports(
            &conn,
            c,
            &[
                Import {
                    module_path: "crate::a".into(),
                    alias: None,
                    file: PathBuf::new(),
                },
                Import {
                    module_path: "crate::b".into(),
                    alias: None,
                    file: PathBuf::new(),
                },
            ],
        )
        .unwrap();
        queries::insert_references(
            &conn,
            c,
            &[Reference {
                name: "serve".into(),
                file: PathBuf::new(),
                start_line: 9,
                start_col: 10,
                kind: ReferenceKind::Path,
                container: "run".into(),
                qualifier: String::new(),
            }],
        )
        .unwrap();

        // Either target alone would be a guess: both stay empty.
        for module in ["crate::a", "crate::b"] {
            let precise = find_callers(&conn, "serve", Some(module)).unwrap();
            assert!(
                precise.is_empty(),
                "tied Path ref must not attribute to {module}: {precise:?}"
            );
        }
        // Unflagged name matching still reports the site (low confidence).
        let all = find_callers(&conn, "serve", None).unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn find_callers_uses_qualifier_to_break_module_ties() {
        let conn = setup();
        for (file, module) in [("src/a.rs", "crate::a"), ("src/b.rs", "crate::b")] {
            let id = queries::insert_file(
                &conn,
                &FileNode {
                    path: PathBuf::from(file),
                    content_hash: "h".into(),
                },
            )
            .unwrap();
            queries::insert_symbols(
                &conn,
                id,
                &[Symbol {
                    name: "serve".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: 1,
                    start_col: 1,
                    module_path: module.into(),
                }],
            )
            .unwrap();
        }
        // c.rs imports both modules but qualifies each call site explicitly.
        let c = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("src/c.rs"),
                content_hash: "hc".into(),
            },
        )
        .unwrap();
        queries::insert_imports(
            &conn,
            c,
            &[
                Import {
                    module_path: "crate::a".into(),
                    alias: None,
                    file: PathBuf::new(),
                },
                Import {
                    module_path: "crate::b".into(),
                    alias: None,
                    file: PathBuf::new(),
                },
            ],
        )
        .unwrap();
        for (line, qualifier) in [(9u32, "a"), (10u32, "b")] {
            queries::insert_references(
                &conn,
                c,
                &[Reference {
                    name: "serve".into(),
                    file: PathBuf::new(),
                    start_line: line,
                    start_col: 5,
                    kind: ReferenceKind::Path,
                    container: "run".into(),
                    qualifier: qualifier.into(),
                }],
            )
            .unwrap();
        }

        let to_a = find_callers(&conn, "serve", Some("crate::a")).unwrap();
        assert_eq!(to_a.len(), 1);
        assert_eq!(to_a[0].start_line, 9);
        let to_b = find_callers(&conn, "serve", Some("crate::b")).unwrap();
        assert_eq!(to_b.len(), 1);
        assert_eq!(to_b[0].start_line, 10);
    }

    #[test]
    fn find_alias_uses_maps_aliases_to_their_module() {
        let conn = setup();
        for (file, module) in [("a.ts", "a"), ("b.ts", "b")] {
            let id = queries::insert_file(
                &conn,
                &FileNode {
                    path: PathBuf::from(file),
                    content_hash: "h".into(),
                },
            )
            .unwrap();
            queries::insert_symbols(
                &conn,
                id,
                &[Symbol {
                    name: "serve".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: 1,
                    start_col: 1,
                    module_path: module.into(),
                }],
            )
            .unwrap();
        }
        let c = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("c.ts"),
                content_hash: "hc".into(),
            },
        )
        .unwrap();
        queries::insert_imports(
            &conn,
            c,
            &[Import {
                module_path: "a::serve".into(),
                alias: Some("serveA".into()),
                file: PathBuf::new(),
            }],
        )
        .unwrap();
        queries::insert_references(
            &conn,
            c,
            &[Reference {
                name: "serveA".into(),
                file: PathBuf::new(),
                start_line: 3,
                start_col: 1,
                kind: ReferenceKind::Call,
                container: String::new(),
                qualifier: String::new(),
            }],
        )
        .unwrap();

        // Unflagged: the alias maps into a defining module.
        let uses = find_alias_uses(&conn, "serve", None).unwrap();
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].name, "serveA");
        // Flagged: only the mapped module matches.
        let uses = find_alias_uses(&conn, "serve", Some("a")).unwrap();
        assert_eq!(uses.len(), 1);
        let uses = find_alias_uses(&conn, "serve", Some("b")).unwrap();
        assert!(uses.is_empty(), "got {uses:?}");
        // Unknown names have no aliases.
        let uses = find_alias_uses(&conn, "nope", None).unwrap();
        assert!(uses.is_empty());
    }

    #[test]
    fn find_alias_uses_skips_shadowed_aliases() {
        let conn = setup();
        let a = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("a.py"),
                content_hash: "h".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            &conn,
            a,
            &[Symbol {
                name: "save".into(),
                kind: SymbolKind::Function,
                file: PathBuf::new(),
                start_line: 1,
                start_col: 1,
                module_path: "a".into(),
            }],
        )
        .unwrap();
        // c.py imports the alias but redefines it locally: the local wins.
        let c = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("c.py"),
                content_hash: "hc".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            &conn,
            c,
            &[Symbol {
                name: "backup".into(),
                kind: SymbolKind::Function,
                file: PathBuf::new(),
                start_line: 9,
                start_col: 1,
                module_path: "c".into(),
            }],
        )
        .unwrap();
        queries::insert_imports(
            &conn,
            c,
            &[Import {
                module_path: "a::save".into(),
                alias: Some("backup".into()),
                file: PathBuf::new(),
            }],
        )
        .unwrap();
        queries::insert_references(
            &conn,
            c,
            &[Reference {
                name: "backup".into(),
                file: PathBuf::new(),
                start_line: 12,
                start_col: 5,
                kind: ReferenceKind::Call,
                container: "run".into(),
                qualifier: String::new(),
            }],
        )
        .unwrap();

        let uses = find_alias_uses(&conn, "save", None).unwrap();
        assert!(uses.is_empty(), "shadowed alias must not match: {uses:?}");
    }

    #[test]
    fn find_callers_uses_method_receiver_to_break_module_ties() {
        let conn = setup();
        // Dotted (Python-style) modules both defining `save`.
        for (file, module) in [("a.py", "a"), ("b.py", "b")] {
            let id = queries::insert_file(
                &conn,
                &FileNode {
                    path: PathBuf::from(file),
                    content_hash: "h".into(),
                },
            )
            .unwrap();
            queries::insert_symbols(
                &conn,
                id,
                &[Symbol {
                    name: "save".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: 1,
                    start_col: 1,
                    module_path: module.into(),
                }],
            )
            .unwrap();
        }
        let c = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("c.py"),
                content_hash: "hc".into(),
            },
        )
        .unwrap();
        queries::insert_imports(
            &conn,
            c,
            &[
                Import {
                    module_path: "a".into(),
                    alias: None,
                    file: PathBuf::new(),
                },
                Import {
                    module_path: "b".into(),
                    alias: None,
                    file: PathBuf::new(),
                },
            ],
        )
        .unwrap();
        queries::insert_references(
            &conn,
            c,
            &[Reference {
                name: "save".into(),
                file: PathBuf::new(),
                start_line: 5,
                start_col: 5,
                kind: ReferenceKind::Method,
                container: "run".into(),
                qualifier: "a".into(),
            }],
        )
        .unwrap();

        let to_a = find_callers(&conn, "save", Some("a")).unwrap();
        assert_eq!(to_a.len(), 1);
        let to_b = find_callers(&conn, "save", Some("b")).unwrap();
        assert!(to_b.is_empty(), "got {to_b:?}");
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rank_prefers_case_match_then_substring_then_typo() {
        let candidates = names(&["create_order", "AuthService", "cancel_order", "zzz"]);
        // Case-only difference wins outright.
        assert_eq!(
            rank_candidates("authservice", &candidates, 3),
            vec!["AuthService".to_string()]
        );
        // Substring beats edit distance; ties break by name.
        assert_eq!(
            rank_candidates("order", &candidates, 3),
            vec!["cancel_order".to_string(), "create_order".to_string()]
        );
        // Single transposition is within the length-scaled threshold.
        assert_eq!(
            rank_candidates("craete_order", &candidates, 3),
            vec!["create_order".to_string()]
        );
    }

    #[test]
    fn rank_suggests_overqualified_prefix() {
        let candidates = names(&["AuthService", "create_order"]);
        // Over-qualified guess recovers the indexed prefix.
        assert_eq!(
            rank_candidates("AuthServiceFactory", &candidates, 3),
            vec!["AuthService".to_string()]
        );
        // …but a short name inside long nonsense does not suggest.
        assert!(rank_candidates(
            "NonexistentSymbolXYZ123",
            &names(&["Symbol"]),
            3
        )
        .is_empty());
    }

    #[test]
    fn rank_rejects_noise() {
        let candidates = names(&["AuthService", "create_order", "id", "db"]);
        // Nonsense stays a clean miss even with short symbols indexed.
        assert!(rank_candidates("NonexistentSymbolXYZ123", &candidates, 3).is_empty());
        // Single-char queries never suggest.
        assert!(rank_candidates("e", &candidates, 3).is_empty());
        assert!(suggest_names(&setup(), "e", 3).unwrap().is_empty());
    }

    #[test]
    fn suggest_names_reads_index() {
        let conn = setup();
        let file = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("src/lib.rs"),
                content_hash: "h".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            &conn,
            file,
            &[Symbol {
                name: "AuthService".into(),
                kind: SymbolKind::Struct,
                file: PathBuf::new(),
                start_line: 1,
                start_col: 1,
                module_path: "crate".into(),
            }],
        )
        .unwrap();
        assert_eq!(
            suggest_names(&conn, "authservice", 3).unwrap(),
            vec!["AuthService".to_string()]
        );
        assert!(suggest_names(&conn, "NonexistentSymbolXYZ", 3)
            .unwrap()
            .is_empty());
    }
}
