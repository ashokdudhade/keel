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
use crate::graph::types::{Reference, Symbol};
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
/// (1 = import, 2 = same-module, 3 = name-only fallback).
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
        let ranked = resolve_definition(conn, name, from)?;
        let Some(top) = ranked.first() else {
            continue;
        };
        if top.module_path != target_module {
            continue;
        }
        if require_precise {
            let tier = rank_tier(top, name, from, &imports, &file_modules);
            if tier > 2 {
                continue;
            }
        }
        out.push(r);
    }
    Ok(out)
}

fn rank_tier(
    sym: &Symbol,
    name: &str,
    from_file_or_module: &str,
    imports: &[(String, Option<String>)],
    file_modules: &[String],
) -> u8 {
    if import_reaches(sym, name, imports) {
        return 1;
    }
    if same_module(sym, from_file_or_module, file_modules) {
        return 2;
    }
    3
}

/// Tier 1: import path equals `module_path::name`, or alias equals the lookup name
/// while the import path points at that qualified symbol (or its module).
fn import_reaches(sym: &Symbol, name: &str, imports: &[(String, Option<String>)]) -> bool {
    let qualified = qualified_name(&sym.module_path, &sym.name);
    for (module_path, alias) in imports {
        if let Some(alias) = alias {
            if alias == name
                && (module_path == &qualified
                    || import_matches_module(module_path, &sym.module_path)
                    || (path_ends_with_name(module_path, &sym.name)
                        && import_matches_module(module_path_prefix(module_path), &sym.module_path)))
            {
                return true;
            }
        } else if module_path == &qualified
            || (import_matches_module(module_path, &sym.module_path) && sym.name == name)
            || (path_ends_with_name(module_path, name)
                && import_matches_module(module_path_prefix(module_path), &sym.module_path))
        {
            return true;
        }
    }
    false
}

/// True when an import path identifies `module_path` (exact, or Go-style last
/// path segment / trailing `::` segment).
fn import_matches_module(import_path: &str, module_path: &str) -> bool {
    if import_path == module_path {
        return true;
    }
    if import_path.rsplit('/').next() == Some(module_path) {
        return true;
    }
    if import_path.rsplit("::").next() == Some(module_path) {
        return true;
    }
    false
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
}
