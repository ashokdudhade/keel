//! Dependency graph: modules/files a target depends on.

use crate::db::queries;
use crate::error::Result;
use crate::graph::resolve;
use rusqlite::Connection;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// A module (and optional defining file) that a target depends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    /// Qualified module path of the dependency (e.g. `crate::b`).
    pub module_path: String,
    /// A file that defines symbols in that module, when known.
    pub file: Option<PathBuf>,
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
            let Some(top) = resolve::acceptable_top_match(&ranked) else {
                continue;
            };
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
        .map(|(module_path, file)| Dependency { module_path, file })
        .collect())
}

/// Collect file paths belonging to `target` (module path and/or symbol name).
fn files_for_target(conn: &Connection, target: &str) -> Result<Vec<String>> {
    let mut files = queries::files_for_module_path(conn, target)?;
    for sym in queries::find_definition(conn, target)? {
        let path = sym.file.to_string_lossy().into_owned();
        if !files.contains(&path) {
            files.push(path);
        }
    }
    files.sort();
    files.dedup();
    Ok(files)
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
}
