//! Normalize query targets (symbol name, module path, or file path) to files.

use crate::db::queries;
use crate::error::Result;
use rusqlite::{params, Connection, OptionalExtension};

/// Files and optional preferred module identity for a query target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    /// Indexed file paths belonging to the target, sorted and de-duplicated.
    pub files: Vec<String>,
    /// Preferred module path when known (exact module query, or unique def module).
    pub preferred_module: Option<String>,
}

/// Resolve `target` to indexed files.
///
/// Acceptance order:
/// 1. Exact file path present in `files`
/// 2. Exact `module_path` with defining files
/// 3. Symbol name definitions
pub fn normalize_target(conn: &Connection, target: &str) -> Result<ResolvedTarget> {
    if file_indexed(conn, target)? {
        let modules = queries::module_paths_in_file(conn, target)?;
        return Ok(ResolvedTarget {
            files: vec![target.to_string()],
            preferred_module: preferred_module_from_list(&modules),
        });
    }

    let mod_files = queries::files_for_module_path(conn, target)?;
    if !mod_files.is_empty() {
        return Ok(ResolvedTarget {
            files: mod_files,
            preferred_module: Some(target.to_string()),
        });
    }

    let defs = queries::find_definition(conn, target)?;
    let mut files = Vec::new();
    for d in &defs {
        let path = d.file.to_string_lossy().into_owned();
        if !files.contains(&path) {
            files.push(path);
        }
    }
    files.sort();
    Ok(ResolvedTarget {
        preferred_module: unique_module_from_symbols(&defs),
        files,
    })
}

fn file_indexed(conn: &Connection, path: &str) -> Result<bool> {
    let id: Option<i64> = conn
        .query_row(
            "SELECT id FROM files WHERE path = ?1",
            params![path],
            |row| row.get(0),
        )
        .optional()?;
    Ok(id.is_some())
}

fn preferred_module_from_list(modules: &[String]) -> Option<String> {
    match modules {
        [] => None,
        [only] => Some(only.clone()),
        rest => {
            // Prefer the shortest path (parent module) when a file declares several.
            rest.iter().min_by_key(|m| m.len()).cloned()
        }
    }
}

fn unique_module_from_symbols(defs: &[crate::graph::types::Symbol]) -> Option<String> {
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
    use crate::db::{queries, schema};
    use crate::graph::types::{FileNode, Symbol, SymbolKind};
    use std::path::PathBuf;

    fn setup_mcp_fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        let id = queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("src/mcp/mod.rs"),
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
                module_path: "crate::mcp".into(),
            }],
        )
        .unwrap();
        conn
    }

    #[test]
    fn normalize_accepts_module_path_and_file() {
        let conn = setup_mcp_fixture();
        let by_mod = normalize_target(&conn, "crate::mcp").unwrap();
        assert!(by_mod.files.iter().any(|f| f.ends_with("mcp/mod.rs")));
        assert_eq!(by_mod.preferred_module.as_deref(), Some("crate::mcp"));

        let by_file = normalize_target(&conn, "src/mcp/mod.rs").unwrap();
        assert_eq!(by_mod.files, by_file.files);
        assert_eq!(by_file.preferred_module.as_deref(), Some("crate::mcp"));
    }

    #[test]
    fn normalize_accepts_symbol_name() {
        let conn = setup_mcp_fixture();
        let by_sym = normalize_target(&conn, "serve").unwrap();
        assert_eq!(by_sym.files, vec!["src/mcp/mod.rs".to_string()]);
        assert_eq!(by_sym.preferred_module.as_deref(), Some("crate::mcp"));
    }
}
