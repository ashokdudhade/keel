//! Dependency graph: modules/files a target depends on.

use crate::db::queries;
use crate::error::Result;
use crate::graph::resolve;
use rusqlite::Connection;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// A module (and optional defining file) that a target depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    /// Qualified module path of the dependency (e.g. `crate::b`).
    pub module_path: String,
    /// A file that defines symbols in that module, when known.
    pub file: Option<PathBuf>,
    /// True when no defining file is indexed (stdlib / third-party).
    pub external: bool,
}

/// Find modules the `target` depends on.
///
/// `target` may be a module path (`crate::a`) or a symbol name. Dependencies
/// are derived from `imports` on the target's files and from references that
/// resolve to symbols in other files. Results are de-duplicated and ordered
/// by `module_path`.
pub fn find_dependencies(conn: &Connection, target: &str) -> Result<Vec<Dependency>> {
    let files = files_for_target(conn, target)?;
    let mut deps: BTreeMap<String, Option<PathBuf>> = BTreeMap::new();

    for file in &files {
        for (module_path, _) in queries::imports_for_file(conn, file)? {
            let dep_mod = normalize_import_module(conn, &module_path)?;
            if deps.contains_key(&dep_mod) {
                continue;
            }
            let dep_file = queries::first_file_for_module_path(conn, &dep_mod)?;
            deps.insert(dep_mod, dep_file);
        }

        for name in queries::reference_names_in_file(conn, file)? {
            let ranked = resolve::resolve_definition_ranked(conn, &name, file)?;
            // Dependency edges need evidence: import-backed (tier 1) or
            // same-module (tier 2). The tier-3 single-name fallback stays
            // available to impact (a candidate list by design), but here it
            // fabricates edges — e.g. a `client.get()` call matching the only
            // free function named `get` in an unrelated test file.
            let Some((tier, top)) = ranked.first() else {
                continue;
            };
            if *tier > 2 {
                continue;
            }
            if same_path(&top.file, file) {
                continue;
            }
            let dep_mod = top.module_path.clone();
            if dep_mod.is_empty() || deps.contains_key(&dep_mod) {
                continue;
            }
            deps.insert(dep_mod, Some(top.file.clone()));
        }
    }

    Ok(deps
        .into_iter()
        .map(|(module_path, file)| Dependency {
            external: file.is_none(),
            module_path,
            file,
        })
        .collect())
}

/// Find modules that depend on `target` (reverse dependencies).
///
/// `target` may be a module path (`crate::b`), a file path, or a symbol name.
/// A module counts as a dependent when one of its files imports a target
/// module (normalized like [`find_dependencies`]) or holds a reference that
/// resolves to a target-module symbol at tier ≤ 2 — the same evidence bar as
/// forward edges, so the tier-3 single-name fallback can't fabricate edges.
/// The target's own modules never list themselves. Results are de-duplicated
/// and ordered by `module_path`.
pub fn find_dependents(conn: &Connection, target: &str) -> Result<Vec<Dependency>> {
    let resolved = crate::graph::target::normalize_target(conn, target)?;
    let mut target_modules = BTreeSet::new();
    for file in &resolved.files {
        for module in queries::module_paths_in_file(conn, file)? {
            if !module.is_empty() {
                target_modules.insert(module);
            }
        }
    }
    if target_modules.is_empty() {
        return Ok(Vec::new());
    }
    let mut dependents: BTreeMap<String, Option<PathBuf>> = BTreeMap::new();
    for file in queries::indexed_files(conn)? {
        let mut evidence = false;
        for (module_path, _) in queries::imports_for_file(conn, &file)? {
            let dep_mod = normalize_import_module(conn, &module_path)?;
            if target_modules.contains(&dep_mod) {
                evidence = true;
                break;
            }
        }
        if !evidence {
            for name in queries::reference_names_in_file(conn, &file)? {
                let ranked = resolve::resolve_definition_ranked(conn, &name, &file)?;
                let Some((tier, top)) = ranked.first() else {
                    continue;
                };
                if *tier > 2 {
                    continue;
                }
                if same_path(&top.file, &file) {
                    continue;
                }
                if target_modules.contains(&top.module_path) {
                    evidence = true;
                    break;
                }
            }
        }
        if !evidence {
            continue;
        }
        let mut modules = queries::module_paths_in_file(conn, &file)?;
        // Top-level-only scripts and barrels define no symbols, so derive
        // the file's own module from its path instead of dropping the
        // evidence; the evidence file itself is the defining file.
        let mut fallback_file: Option<PathBuf> = None;
        if modules.is_empty() {
            if let Some(module) = fallback_module_for_file(&file) {
                fallback_file = Some(PathBuf::from(&file));
                modules = vec![module];
            }
        }
        for module in modules {
            if module.is_empty() || target_modules.contains(&module) {
                continue;
            }
            if dependents.contains_key(&module) {
                continue;
            }
            let dep_file = match &fallback_file {
                Some(f) => Some(f.clone()),
                None => queries::first_file_for_module_path(conn, &module)?,
            };
            dependents.insert(module, dep_file);
        }
    }

    Ok(dependents
        .into_iter()
        .map(|(module_path, file)| Dependency {
            external: file.is_none(),
            module_path,
            file,
        })
        .collect())
}

/// Collect file paths belonging to `target` (module path, file path, and/or symbol name).
fn files_for_target(conn: &Connection, target: &str) -> Result<Vec<String>> {
    Ok(crate::graph::target::normalize_target(conn, target)?.files)
}

/// Map an import path to a dependency module.
///
/// Prefer the longest prefix that has indexed symbols. Item imports like
/// `crate::b::f` therefore collapse to `crate::b` when only that module exists;
/// module imports (`crate::b`) are kept as-is.
fn normalize_import_module(conn: &Connection, import: &str) -> Result<String> {
    if queries::first_file_for_module_path(conn, import)?.is_some() {
        return Ok(import.to_string());
    }
    let mut candidate = import;
    while let Some((parent, _)) = candidate.rsplit_once("::") {
        if queries::first_file_for_module_path(conn, parent)?.is_some() {
            return Ok(parent.to_string());
        }
        candidate = parent;
    }
    Ok(import.to_string())
}

fn same_path(a: &Path, b: &str) -> bool {
    a == Path::new(b)
}

/// Module identity for an evidence file that defines no symbols.
/// Shared with target resolution ([`crate::languages::path_module_fallback`]).
fn fallback_module_for_file(file: &str) -> Option<String> {
    crate::languages::path_module_fallback(Path::new(file))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{queries, schema};
    use crate::graph::types::{FileNode, Import, Reference, ReferenceKind, Symbol, SymbolKind};

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        conn
    }

    /// `mod a` imports `crate::b` and calls `b::f`; `leaf` has no imports.
    fn fixture_a_depends_on_b(conn: &Connection) {
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
                name: "g".into(),
                kind: SymbolKind::Function,
                file: PathBuf::new(),
                start_line: 3,
                start_col: 1,
                module_path: "crate::a".into(),
                container: String::new(),
            }],
        )
        .unwrap();
        queries::insert_imports(
            conn,
            a,
            &[
                Import {
                    module_path: "crate::b".into(),
                    alias: None,
                    file: PathBuf::new(),
                },
                // Duplicate import path — must de-dupe.
                Import {
                    module_path: "crate::b".into(),
                    alias: Some("bb".into()),
                    file: PathBuf::new(),
                },
                Import {
                    module_path: "crate::c".into(),
                    alias: None,
                    file: PathBuf::new(),
                },
            ],
        )
        .unwrap();
        queries::insert_references(
            conn,
            a,
            &[Reference {
                name: "f".into(),
                file: PathBuf::new(),
                start_line: 4,
                start_col: 5,
                kind: ReferenceKind::Call,
                container: "crate::a::g".into(),
                qualifier: String::new(),
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
                name: "f".into(),
                kind: SymbolKind::Function,
                file: PathBuf::new(),
                start_line: 1,
                start_col: 1,
                module_path: "crate::b".into(),
                container: String::new(),
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
        queries::insert_symbols(
            conn,
            c,
            &[Symbol {
                name: "h".into(),
                kind: SymbolKind::Function,
                file: PathBuf::new(),
                start_line: 1,
                start_col: 1,
                module_path: "crate::c".into(),
                container: String::new(),
            }],
        )
        .unwrap();

        let leaf = queries::insert_file(
            conn,
            &FileNode {
                path: PathBuf::from("src/leaf.rs"),
                content_hash: "hl".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            conn,
            leaf,
            &[Symbol {
                name: "alone".into(),
                kind: SymbolKind::Function,
                file: PathBuf::new(),
                start_line: 1,
                start_col: 1,
                module_path: "crate::leaf".into(),
                container: String::new(),
            }],
        )
        .unwrap();
    }

    #[test]
    fn find_dependencies_includes_imported_module() {
        let conn = setup();
        fixture_a_depends_on_b(&conn);

        let deps = find_dependencies(&conn, "crate::a").unwrap();
        let paths: Vec<&str> = deps.iter().map(|d| d.module_path.as_str()).collect();
        assert!(
            paths.contains(&"crate::b"),
            "expected crate::b in {paths:?}"
        );
        let b = deps.iter().find(|d| d.module_path == "crate::b").unwrap();
        assert_eq!(b.file, Some(PathBuf::from("src/b.rs")));
    }

    #[test]
    fn find_dependencies_leaf_module_returns_empty() {
        let conn = setup();
        fixture_a_depends_on_b(&conn);

        let deps = find_dependencies(&conn, "crate::leaf").unwrap();
        assert!(deps.is_empty(), "leaf must have no deps: {deps:?}");
    }

    #[test]
    fn find_dependencies_ordered_and_deduped() {
        let conn = setup();
        fixture_a_depends_on_b(&conn);

        let deps = find_dependencies(&conn, "crate::a").unwrap();
        let paths: Vec<&str> = deps.iter().map(|d| d.module_path.as_str()).collect();
        assert_eq!(paths, vec!["crate::b", "crate::c"]);
    }

    #[test]
    fn find_dependencies_by_symbol_name() {
        let conn = setup();
        fixture_a_depends_on_b(&conn);

        let deps = find_dependencies(&conn, "g").unwrap();
        let paths: Vec<&str> = deps.iter().map(|d| d.module_path.as_str()).collect();
        assert!(paths.contains(&"crate::b"), "got {paths:?}");
    }

    /// `u` calls a method `get` it never imports; the only `get` defined
    /// anywhere is an unrelated free function in `t` (the fastapi-crawler
    /// `client.get` vs test-helper `get` repro: `dependencies main` listed
    /// `tests.test_main`, which `main.py` never imports).
    fn fixture_unrelated_name_match(conn: &Connection) {
        let u = queries::insert_file(
            conn,
            &FileNode {
                path: PathBuf::from("src/u.rs"),
                content_hash: "hu".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            conn,
            u,
            &[Symbol {
                name: "u_main".into(),
                kind: SymbolKind::Function,
                file: PathBuf::new(),
                start_line: 1,
                start_col: 1,
                module_path: "crate::u".into(),
                container: String::new(),
            }],
        )
        .unwrap();
        queries::insert_references(
            conn,
            u,
            &[Reference {
                name: "get".into(),
                file: PathBuf::new(),
                start_line: 2,
                start_col: 5,
                kind: ReferenceKind::Method,
                container: "crate::u::u_main".into(),
                qualifier: String::new(),
            }],
        )
        .unwrap();

        let t = queries::insert_file(
            conn,
            &FileNode {
                path: PathBuf::from("src/t.rs"),
                content_hash: "ht".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            conn,
            t,
            &[Symbol {
                name: "get".into(),
                kind: SymbolKind::Function,
                file: PathBuf::new(),
                start_line: 1,
                start_col: 1,
                module_path: "crate::t".into(),
                container: String::new(),
            }],
        )
        .unwrap();
    }

    #[test]
    fn find_dependencies_ignores_unimported_name_matches() {
        let conn = setup();
        fixture_unrelated_name_match(&conn);

        let deps = find_dependencies(&conn, "crate::u").unwrap();
        let paths: Vec<&str> = deps.iter().map(|d| d.module_path.as_str()).collect();
        assert!(
            !paths.contains(&"crate::t"),
            "unimported name match must not be an edge, got {paths:?}"
        );
        assert!(deps.is_empty(), "expected no deps, got {paths:?}");
    }

    #[test]
    fn find_dependents_lists_importing_modules() {
        let conn = setup();
        fixture_a_depends_on_b(&conn);

        let deps = find_dependents(&conn, "crate::b").unwrap();
        let paths: Vec<&str> = deps.iter().map(|d| d.module_path.as_str()).collect();
        assert_eq!(paths, vec!["crate::a"]);
        assert_eq!(deps[0].file, Some(PathBuf::from("src/a.rs")));
        assert!(!deps[0].external);
    }

    #[test]
    fn find_dependents_by_symbol_and_file_target() {
        let conn = setup();
        fixture_a_depends_on_b(&conn);

        for target in ["f", "src/b.rs"] {
            let deps = find_dependents(&conn, target).unwrap();
            let paths: Vec<&str> = deps.iter().map(|d| d.module_path.as_str()).collect();
            assert_eq!(paths, vec!["crate::a"], "for target {target}");
        }
    }

    #[test]
    fn find_dependents_empty_and_never_self_lists() {
        let conn = setup();
        fixture_a_depends_on_b(&conn);

        let deps = find_dependents(&conn, "crate::leaf").unwrap();
        assert!(deps.is_empty(), "leaf has no dependents: {deps:?}");

        // `a` imports others, but nothing imports `a`: no self-listing.
        let deps = find_dependents(&conn, "crate::a").unwrap();
        assert!(deps.is_empty(), "must not self-list: {deps:?}");
    }

    #[test]
    fn find_dependents_ignores_unimported_name_matches() {
        let conn = setup();
        fixture_unrelated_name_match(&conn);

        // `u` calls `get` without importing `crate::t`: tier-3 only, no edge.
        let deps = find_dependents(&conn, "crate::t").unwrap();
        assert!(deps.is_empty(), "expected no dependents, got {deps:?}");
    }

    #[test]
    fn find_dependents_lists_symbol_less_importing_files() {
        let conn = setup();
        // `pkg/a.py` defines `helper`; `main.py` only imports and calls it
        // at top level (no symbols of its own).
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
                name: "helper".into(),
                kind: SymbolKind::Function,
                file: PathBuf::new(),
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
        queries::insert_imports(
            &conn,
            main,
            &[Import {
                module_path: "pkg.a::helper".into(),
                alias: None,
                file: PathBuf::new(),
            }],
        )
        .unwrap();

        let deps = find_dependents(&conn, "pkg.a").unwrap();
        let paths: Vec<&str> = deps.iter().map(|d| d.module_path.as_str()).collect();
        assert_eq!(paths, vec!["main"]);
        assert!(!deps[0].external);
        assert_eq!(deps[0].file, Some(PathBuf::from("main.py")));
    }

    #[test]
    fn find_dependencies_marks_unindexed_modules_external() {
        let conn = setup();
        fixture_a_depends_on_b(&conn);

        // An import nothing defines (stdlib-style): external, no file.
        let a_id: i64 = conn
            .query_row(
                "SELECT id FROM files WHERE path = 'src/a.rs'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        queries::insert_imports(
            &conn,
            a_id,
            &[Import {
                module_path: "std::collections".into(),
                alias: None,
                file: PathBuf::new(),
            }],
        )
        .unwrap();

        let deps = find_dependencies(&conn, "crate::a").unwrap();
        let z = deps
            .iter()
            .find(|d| d.module_path == "std::collections")
            .unwrap();
        assert!(z.external);
        assert_eq!(z.file, None);

        let b = deps.iter().find(|d| d.module_path == "crate::b").unwrap();
        assert!(!b.external);
        assert_eq!(b.file, Some(PathBuf::from("src/b.rs")));
    }
}
