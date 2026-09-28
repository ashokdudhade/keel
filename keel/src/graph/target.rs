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
/// 2. Module or subtree (`crate::graph` covers `crate::graph::resolve` too:
///    dependents of a parent mean the whole tree, not just `mod.rs`).
///    Beats directory: TypeScript modules like `src/auth` are slashy
///    but exact.
/// 3. Directory prefix over indexed files (`src/graph`, trailing `/` ok)
/// 4. Qualified symbol (`module::…::name`, e.g. `crate::mcp::serve`)
/// 5. Symbol name definitions
pub fn normalize_target(conn: &Connection, target: &str) -> Result<ResolvedTarget> {
    if file_indexed(conn, target)? {
        let modules = queries::module_paths_in_file(conn, target)?;
        return Ok(ResolvedTarget {
            files: vec![target.to_string()],
            preferred_module: preferred_module_from_list(&modules),
        });
    }

    let subtree = files_for_module_subtree(conn, target)?;
    if !subtree.is_empty() {
        return Ok(ResolvedTarget {
            files: subtree,
            preferred_module: Some(target.to_string()),
        });
    }

    if let Some(dir) = directory_files(conn, target)? {
        let mut modules = Vec::new();
        for f in &dir {
            for m in queries::module_paths_in_file(conn, f)? {
                if !modules.contains(&m) {
                    modules.push(m);
                }
            }
        }
        return Ok(ResolvedTarget {
            files: dir,
            preferred_module: preferred_module_from_list(&modules),
        });
    }

    // Symbol-less files (top-level scripts, barrels) match by their
    // path-derived module identity. Module-shaped targets beat symbol
    // names, mirroring the subtree-first order above.
    let fallback = files_for_fallback_module(conn, target)?;
    if !fallback.is_empty() {
        return Ok(ResolvedTarget {
            files: fallback,
            preferred_module: Some(target.to_string()),
        });
    }

    if let Some((module, bare)) = target.rsplit_once("::") {
        let qualified = queries::find_definition_by_qualified(conn, module, bare)?;
        if !qualified.is_empty() {
            let mut files = Vec::new();
            for d in &qualified {
                let path = d.file.to_string_lossy().into_owned();
                if !files.contains(&path) {
                    files.push(path);
                }
            }
            files.sort();
            return Ok(ResolvedTarget {
                files,
                preferred_module: Some(module.to_string()),
            });
        }
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

/// Indexed files under `target` when it names a directory (trailing `/` and
/// leading `./` tolerated, separators normalized). `Some` only when at
/// least one indexed file sits under the prefix; `.`/empty never match
/// (dependents of "everything" is not a useful query).
fn directory_files(conn: &Connection, target: &str) -> Result<Option<Vec<String>>> {
    let trimmed = target.trim_end_matches('/').trim_start_matches("./");
    if trimmed.is_empty() || trimmed == "." {
        return Ok(None);
    }
    let prefix = format!("{}/", trimmed.replace('\\', "/"));
    let mut out = Vec::new();
    for f in queries::indexed_files(conn)? {
        if f.replace('\\', "/").starts_with(&prefix) {
            out.push(f);
        }
    }
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(out))
    }
}

/// Files defining `target` or any module below it (`crate::graph` covers
/// `crate::graph::resolve`). Nesting separators are per-language (`::` for
/// Rust, `.` for Python, `/` for Go paths), all separator-anchored so `src`
/// never matches `srcx.y`. Empty when nothing matches. `LIKE`
/// metacharacters in the target are escaped (module paths contain `_`).
fn files_for_module_subtree(conn: &Connection, target: &str) -> Result<Vec<String>> {
    let escaped: String = target
        .chars()
        .flat_map(|c| match c {
            '\\' | '%' | '_' => vec!['\\', c],
            _ => vec![c],
        })
        .collect();
    let like_cc = format!("{escaped}::%");
    let like_dot = format!("{escaped}.%");
    let like_slash = format!("{escaped}/%");
    let mut stmt = conn.prepare(
        "SELECT DISTINCT f.path FROM symbols s JOIN files f ON s.file_id = f.id
         WHERE s.module_path = ?1
            OR s.module_path LIKE ?2 ESCAPE '\\'
            OR s.module_path LIKE ?3 ESCAPE '\\'
            OR s.module_path LIKE ?4 ESCAPE '\\'
         ORDER BY f.path",
    )?;
    let rows = stmt.query_map(
        params![target, like_cc, like_dot, like_slash],
        |row| row.get::<_, String>(0),
    )?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Files for a module/subtree or directory target (outline resolution).
///
/// Unlike [`normalize_target`], this never resolves symbols: outline lists
/// *defined* symbols, so a bare name is a miss here, not a definition lookup.
/// `None` when `target` is neither a module (subtree) nor a directory.
pub(crate) fn module_or_dir_files(
    conn: &Connection,
    target: &str,
) -> Result<Option<Vec<String>>> {
    let subtree = files_for_module_subtree(conn, target)?;
    if !subtree.is_empty() {
        return Ok(Some(subtree));
    }
    if let Some(dir) = directory_files(conn, target)? {
        return Ok(Some(dir));
    }
    let fallback = files_for_fallback_module(conn, target)?;
    if fallback.is_empty() {
        return Ok(None);
    }
    Ok(Some(fallback))
}

/// Files whose path-derived module identity equals `target`, restricted
/// to files with no symbols (gap-fill only: symbol-bearing files resolve
/// through their real modules above, so this never changes them).
fn files_for_fallback_module(conn: &Connection, target: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for file in queries::indexed_files(conn)? {
        if !queries::module_paths_in_file(conn, &file)?.is_empty() {
            continue;
        }
        let identity =
            crate::languages::path_module_fallback(std::path::Path::new(&file));
        if identity.as_deref() == Some(target) {
            out.push(file);
        }
    }
    Ok(out)
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

    #[test]
    fn normalize_accepts_qualified_symbol() {
        let conn = setup_mcp_fixture();
        let by_qualified = normalize_target(&conn, "crate::mcp::serve").unwrap();
        assert_eq!(by_qualified.files, vec!["src/mcp/mod.rs".to_string()]);
        assert_eq!(
            by_qualified.preferred_module.as_deref(),
            Some("crate::mcp")
        );

        // Wrong module: no files, like any other unknown target.
        let miss = normalize_target(&conn, "crate::api::serve").unwrap();
        assert!(miss.files.is_empty());
        assert_eq!(miss.preferred_module, None);
    }

    #[test]
    fn normalize_accepts_directories() {
        let conn = setup_mcp_fixture();
        for dir in ["src/mcp", "src/mcp/", "./src/mcp", "src"] {
            let by_dir = normalize_target(&conn, dir).unwrap();
            assert_eq!(
                by_dir.files,
                vec!["src/mcp/mod.rs".to_string()],
                "target {dir}"
            );
            assert_eq!(by_dir.preferred_module.as_deref(), Some("crate::mcp"));
        }

        // Prefix must end at a segment boundary: `src/mc` is not `src/mcp/`.
        let partial = normalize_target(&conn, "src/mc").unwrap();
        assert!(partial.files.is_empty());

        // Root-ish targets never match everything.
        for root in [".", "./", ""] {
            let miss = normalize_target(&conn, root).unwrap();
            assert!(miss.files.is_empty(), "target {root:?}");
        }
    }

    #[test]
    fn normalize_accepts_module_subtree() {
        let conn = setup_mcp_fixture();
        // `crate` alone defines nothing, but covers `crate::mcp`.
        let by_subtree = normalize_target(&conn, "crate").unwrap();
        assert_eq!(by_subtree.files, vec!["src/mcp/mod.rs".to_string()]);
        assert_eq!(by_subtree.preferred_module.as_deref(), Some("crate"));

        // Exact module still wins over subtree (same files here, exact identity).
        let by_exact = normalize_target(&conn, "crate::mcp").unwrap();
        assert_eq!(
            by_exact.preferred_module.as_deref(),
            Some("crate::mcp")
        );
    }

    #[test]
    fn normalize_accepts_symbol_less_module_by_path_identity() {
        let conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        // `main.py` indexed with no symbols (top-level script).
        queries::insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("main.py"),
                content_hash: "h".into(),
            },
        )
        .unwrap();

        let by_mod = normalize_target(&conn, "main").unwrap();
        assert_eq!(by_mod.files, vec!["main.py".to_string()]);
        assert_eq!(by_mod.preferred_module.as_deref(), Some("main"));

        // Outline resolution sees it as a module too.
        let outline = module_or_dir_files(&conn, "main").unwrap();
        assert_eq!(outline, Some(vec!["main.py".to_string()]));
    }
}
