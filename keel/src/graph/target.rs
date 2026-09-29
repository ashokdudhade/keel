//! Normalize query targets (symbol name, module path, or file path) to files.

use crate::db::queries;
use crate::error::Result;
use crate::graph::types::Symbol;
use rusqlite::{params, Connection, OptionalExtension};

/// Files and optional preferred module identity for a query target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTarget {
    /// Indexed file paths belonging to the target, sorted and de-duplicated.
    pub files: Vec<String>,
    /// Preferred module path when known (exact module query, or unique def module).
    pub preferred_module: Option<String>,
}

/// Split a member-qualified name (`C.method`) into `(container, member)`.
///
/// Only a single trailing `.` segment qualifies: the head must be one
/// identifier (no further `.` or `::`), and both sides must be non-empty.
/// Anything else (bare names, `a.b.c`, `m::C.method`) returns `None` and
/// keeps the existing lookup behavior.
pub fn split_member_name(name: &str) -> Option<(&str, &str)> {
    let (head, member) = name.rsplit_once('.')?;
    if head.is_empty() || member.is_empty() {
        return None;
    }
    if head.contains(['.', ':']) || member.contains(['.', ':']) {
        return None;
    }
    Some((head, member))
}

/// Lexically normalize a path (`./`, `a/../b`) without touching the fs.
///
/// Bails out (returns `raw`) when `..` would escape past the start.
pub(crate) fn clean_path(raw: &str) -> String {
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


/// Resolve a target to the indexed file: exact match, then (for filey
/// targets only) lexically-cleaned match, then a UNIQUE suffix match in
/// either direction — a longer spelling (`../../x.py`, absolute paths)
/// ending at an indexed row, or a bare basename (`l.py` from a subdir)
/// ending an indexed path.
///
/// Pure extensionless identifiers pass through untouched. Ambiguous
/// suffixes resolve to nothing (callers keep trying later acceptance
/// steps or miss honestly). Both directions require a `/` boundary, so
/// dotted modules and member queries can only hit when a real indexed
/// file is literally named that way (their own resolution steps also
/// run first and win).
pub(crate) fn resolve_file_target(conn: &Connection, raw: &str) -> Result<Option<String>> {
    let files = queries::indexed_files(conn)?;
    if files.iter().any(|f| f == raw) {
        return Ok(Some(raw.to_string()));
    }
    // Cleaned/suffix matching only for filey targets: a slash, or a dot
    // (an extension — pure identifiers stay out).
    if !raw.contains(['/', '\\', '.']) {
        return Ok(None);
    }
    let cleaned = clean_path(raw);
    if cleaned != raw && files.iter().any(|f| f == &cleaned) {
        return Ok(Some(cleaned));
    }
    let norm_raw = raw.replace('\\', "/");
    let mut hits = files.iter().filter(|f| {
        let norm_file = f.replace('\\', "/");
        suffix_hit(&norm_raw, &norm_file) || suffix_hit(&norm_file, &norm_raw)
    });
    match (hits.next(), hits.next()) {
        (Some(only), None) => Ok(Some(only.clone())),
        _ => Ok(None),
    }
}

/// True when the longer of two normalized paths ends at the shorter on a
/// `/` boundary (exact equality excluded — the caller tries that first).
fn suffix_hit(longer: &str, shorter: &str) -> bool {
    longer.len() > shorter.len()
        && longer.ends_with(shorter)
        && longer.as_bytes()[longer.len() - shorter.len() - 1] == b'/'
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
/// 4. Suffix-tolerant file (`../../x.py`, absolute paths, bare basenames
///    from subdirs; unique only)
/// 5. Qualified symbol (`module::…::name`, e.g. `crate::mcp::serve`;
///    single-segment `Type::member` falls back to member lookup)
/// 6. Member symbol (`Type.member`)
/// 7. Symbol name definitions
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

    // Suffix-tolerant file match (`../../x.py`, absolute paths, bare
    // basenames from subdirs): runs after module/dir steps so slashy
    // module names keep precedence.
    if let Some(file) = resolve_file_target(conn, target)? {
        let modules = queries::module_paths_in_file(conn, &file)?;
        return Ok(ResolvedTarget {
            files: vec![file],
            preferred_module: preferred_module_from_list(&modules),
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
        // Single-segment `Type::member`: retry as a member lookup, like
        // `definition` (module paths win ties by trying first).
        if !module.is_empty() && !bare.is_empty() && !module.contains("::") {
            let members = queries::find_definition_by_container(conn, module, bare)?;
            if !members.is_empty() {
                return Ok(files_for_symbols(&members));
            }
        }
    }

    // Member-qualified symbols (`Type.member`): the defining files of
    // that member only, so same-named members in other files stay out.
    if let Some((container, sym)) = split_member_name(target) {
        let members = queries::find_definition_by_container(conn, container, sym)?;
        if !members.is_empty() {
            return Ok(files_for_symbols(&members));
        }
    }

    let defs = queries::find_definition(conn, target)?;
    Ok(files_for_symbols(&defs))
}

/// Target from symbol hits: sorted de-duplicated defining files plus the
/// unique module when all hits agree.
fn files_for_symbols(defs: &[Symbol]) -> ResolvedTarget {
    let mut files = Vec::new();
    for d in defs {
        let path = d.file.to_string_lossy().into_owned();
        if !files.contains(&path) {
            files.push(path);
        }
    }
    files.sort();
    ResolvedTarget {
        preferred_module: unique_module_from_symbols(defs),
        files,
    }
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
                container: String::new(),
            }],
        )
        .unwrap();
        conn
    }

    #[test]
    fn normalize_accepts_member_qualified_symbol() {
        let conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        for (path, container, line) in [
            ("a.py", "Alpha", 2),
            ("b.py", "Beta", 2),
        ] {
            let id = queries::insert_file(
                &conn,
                &FileNode {
                    path: PathBuf::from(path),
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
                    start_line: line,
                    start_col: 5,
                    module_path: path.strip_suffix(".py").unwrap().into(),
                    container: container.into(),
                }],
            )
            .unwrap();
        }

        // Dotted member form resolves to the defining file only.
        let dotted = normalize_target(&conn, "Alpha.save").unwrap();
        assert_eq!(dotted.files, vec!["a.py".to_string()]);
        assert_eq!(dotted.preferred_module.as_deref(), Some("a"));

        // Single-segment `::` falls back to the member lookup.
        let scoped = normalize_target(&conn, "Beta::save").unwrap();
        assert_eq!(scoped.files, vec!["b.py".to_string()]);

        // Bare names still cover every defining file.
        let bare = normalize_target(&conn, "save").unwrap();
        assert_eq!(bare.files, vec!["a.py".to_string(), "b.py".to_string()]);
    }

    #[test]
    fn normalize_accepts_relative_and_absolute_file_paths() {
        let conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        for path in ["src/a.py", "src/b.py"] {
            queries::insert_file(
                &conn,
                &FileNode {
                    path: PathBuf::from(path),
                    content_hash: "h".into(),
                },
            )
            .unwrap();
        }

        // Cwd-relative, absolute, and bare-basename spellings find the
        // root-relative row.
        for target in [
            "../../src/a.py",
            "./src/a.py",
            "/repo/src/a.py",
            "a.py",
            "src/../src/a.py",
        ] {
            let got = normalize_target(&conn, target).unwrap();
            assert_eq!(got.files, vec!["src/a.py".to_string()], "{target}");
        }

        // Ambiguous suffixes resolve to nothing (no guessing).
        let conn2 = Connection::open_in_memory().unwrap();
        schema::initialize(&conn2).unwrap();
        for path in ["x/dup.py", "y/dup.py"] {
            queries::insert_file(
                &conn2,
                &FileNode {
                    path: PathBuf::from(path),
                    content_hash: "h".into(),
                },
            )
            .unwrap();
        }
        let ambiguous = normalize_target(&conn2, "sub/dup.py").unwrap();
        assert!(ambiguous.files.is_empty());
        // Ambiguous bare basenames resolve to nothing too.
        let ambiguous_bare = normalize_target(&conn2, "dup.py").unwrap();
        assert!(ambiguous_bare.files.is_empty());

        // Pure extensionless identifiers never take the suffix path.
        let bare = normalize_target(&conn, "a").unwrap();
        assert!(bare.files.is_empty());
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
