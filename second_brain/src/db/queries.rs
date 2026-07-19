//! Type-safe insert/select wrappers over the SQLite tables.

use crate::error::Result;
use crate::graph::types::{
    FileNode, ImplRecord, Import, Reference, ReferenceKind, Symbol, SymbolKind,
};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashMap;
use std::path::PathBuf;

/// Insert a file (or update its hash on conflict) and return its row id.
pub fn insert_file(conn: &Connection, node: &FileNode) -> Result<i64> {
    let path = node.path.to_string_lossy();
    let id: i64 = conn.query_row(
        r#"INSERT INTO files (path, content_hash) VALUES (?1, ?2)
           ON CONFLICT(path) DO UPDATE SET content_hash = excluded.content_hash
           RETURNING id"#,
        params![path, node.content_hash],
        |row| row.get(0),
    )?;
    Ok(id)
}

/// Delete all `symbols`, `references`, `imports`, and `impls` rows for `file_id`.
///
/// Called before re-inserting a file's rows so that re-indexing an unchanged
/// repository is idempotent (identical repo state yields identical query
/// output) instead of accumulating duplicate rows.
pub fn clear_file_rows(conn: &Connection, file_id: i64) -> Result<()> {
    conn.execute("DELETE FROM symbols WHERE file_id = ?1", params![file_id])?;
    conn.execute(
        r#"DELETE FROM "references" WHERE file_id = ?1"#,
        params![file_id],
    )?;
    conn.execute("DELETE FROM imports WHERE file_id = ?1", params![file_id])?;
    conn.execute("DELETE FROM impls WHERE file_id = ?1", params![file_id])?;
    Ok(())
}

/// Load every indexed file's `path -> content_hash` map.
pub fn existing_hashes(conn: &Connection) -> Result<HashMap<String, String>> {
    let mut stmt = conn.prepare("SELECT path, content_hash FROM files")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out = HashMap::new();
    for r in rows {
        let (path, hash) = r?;
        out.insert(path, hash);
    }
    Ok(out)
}

/// Delete a file row and all of its dependent symbol/reference/import/impl rows.
///
/// Foreign keys are declared without `ON DELETE CASCADE`, so child rows are
/// cleared explicitly before the `files` row is removed.
pub fn delete_file_and_rows(conn: &Connection, path: &str) -> Result<()> {
    let file_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM files WHERE path = ?1",
            params![path],
            |row| row.get(0),
        )
        .optional()?;
    let Some(file_id) = file_id else {
        return Ok(());
    };
    clear_file_rows(conn, file_id)?;
    conn.execute("DELETE FROM files WHERE id = ?1", params![file_id])?;
    Ok(())
}

/// Insert all symbols for a file.
pub fn insert_symbols(conn: &Connection, file_id: i64, symbols: &[Symbol]) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT INTO symbols (file_id, name, kind, start_line, start_col, module_path)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for s in symbols {
        stmt.execute(params![
            file_id,
            s.name,
            s.kind.as_db(),
            s.start_line as i64,
            s.start_col as i64,
            s.module_path
        ])?;
    }
    Ok(())
}

/// Insert all references for a file.
pub fn insert_references(conn: &Connection, file_id: i64, references: &[Reference]) -> Result<()> {
    let mut stmt = conn.prepare(
        r#"INSERT INTO "references" (file_id, name, start_line, start_col, kind, container)
           VALUES (?1, ?2, ?3, ?4, ?5, ?6)"#,
    )?;
    for r in references {
        stmt.execute(params![
            file_id,
            r.name,
            r.start_line as i64,
            r.start_col as i64,
            r.kind.as_db(),
            r.container
        ])?;
    }
    Ok(())
}

/// Insert all imports for a file. `alias` is stored as `NULL` when `None`.
pub fn insert_imports(conn: &Connection, file_id: i64, imports: &[Import]) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT INTO imports (file_id, module_path, alias) VALUES (?1, ?2, ?3)",
    )?;
    for i in imports {
        stmt.execute(params![file_id, i.module_path, i.alias])?;
    }
    Ok(())
}

/// Insert all `impl` blocks for a file. `trait_name` is stored as `NULL` when `None`.
pub fn insert_impls(conn: &Connection, file_id: i64, impls: &[ImplRecord]) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT INTO impls (file_id, type_name, trait_name, start_line, start_col)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for i in impls {
        stmt.execute(params![
            file_id,
            i.type_name,
            i.trait_name,
            i.start_line as i64,
            i.start_col as i64
        ])?;
    }
    Ok(())
}

/// Find definitions matching both `module_path` and `name`, ordered by location.
pub fn find_definition_by_qualified(
    conn: &Connection,
    module_path: &str,
    name: &str,
) -> Result<Vec<Symbol>> {
    let mut stmt = conn.prepare(
        "SELECT s.name, s.kind, f.path, s.start_line, s.start_col, s.module_path
         FROM symbols s JOIN files f ON s.file_id = f.id
         WHERE s.module_path = ?1 AND s.name = ?2
         ORDER BY f.path, s.start_line, s.start_col",
    )?;
    let rows = stmt.query_map(params![module_path, name], |row| {
        Ok(Symbol {
            name: row.get::<_, String>(0)?,
            kind: SymbolKind::from_db(&row.get::<_, String>(1)?),
            file: PathBuf::from(row.get::<_, String>(2)?),
            start_line: row.get::<_, i64>(3)? as u32,
            start_col: row.get::<_, i64>(4)? as u32,
            module_path: row.get::<_, String>(5)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Imports recorded for the file at `path` (`module_path`, optional `alias`).
pub fn imports_for_file(conn: &Connection, path: &str) -> Result<Vec<(String, Option<String>)>> {
    let mut stmt = conn.prepare(
        "SELECT i.module_path, i.alias
         FROM imports i JOIN files f ON i.file_id = f.id
         WHERE f.path = ?1
         ORDER BY i.module_path, i.alias",
    )?;
    let rows = stmt.query_map(params![path], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Distinct `module_path` values of symbols defined in the file at `path`.
pub fn module_paths_in_file(conn: &Connection, path: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT s.module_path
         FROM symbols s JOIN files f ON s.file_id = f.id
         WHERE f.path = ?1
         ORDER BY s.module_path",
    )?;
    let rows = stmt.query_map(params![path], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Find all symbol definitions matching `name`, ordered deterministically.
pub fn find_definition(conn: &Connection, name: &str) -> Result<Vec<Symbol>> {
    let mut stmt = conn.prepare(
        "SELECT s.name, s.kind, f.path, s.start_line, s.start_col, s.module_path
         FROM symbols s JOIN files f ON s.file_id = f.id
         WHERE s.name = ?1
         ORDER BY f.path, s.start_line, s.start_col",
    )?;
    let rows = stmt.query_map(params![name], |row| {
        Ok(Symbol {
            name: row.get::<_, String>(0)?,
            kind: SymbolKind::from_db(&row.get::<_, String>(1)?),
            file: PathBuf::from(row.get::<_, String>(2)?),
            start_line: row.get::<_, i64>(3)? as u32,
            start_col: row.get::<_, i64>(4)? as u32,
            module_path: row.get::<_, String>(5)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Find all references matching `name`, ordered deterministically.
pub fn find_references(conn: &Connection, name: &str) -> Result<Vec<Reference>> {
    let mut stmt = conn.prepare(
        r#"SELECT r.name, f.path, r.start_line, r.start_col, r.kind, r.container
           FROM "references" r JOIN files f ON r.file_id = f.id
           WHERE r.name = ?1
           ORDER BY f.path, r.start_line, r.start_col"#,
    )?;
    let rows = stmt.query_map(params![name], |row| {
        Ok(Reference {
            name: row.get::<_, String>(0)?,
            file: PathBuf::from(row.get::<_, String>(1)?),
            start_line: row.get::<_, i64>(2)? as u32,
            start_col: row.get::<_, i64>(3)? as u32,
            kind: ReferenceKind::from_db(&row.get::<_, String>(4)?),
            container: row.get::<_, String>(5)?,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Find trait implementations for `trait_name`, ordered by `(path, line, col)`.
///
/// Inherent impls (`trait_name IS NULL`) are excluded.
pub fn find_implementations(conn: &Connection, trait_name: &str) -> Result<Vec<ImplRecord>> {
    let mut stmt = conn.prepare(
        "SELECT i.type_name, i.trait_name, f.path, i.start_line, i.start_col
         FROM impls i JOIN files f ON i.file_id = f.id
         WHERE i.trait_name = ?1
         ORDER BY f.path, i.start_line, i.start_col",
    )?;
    let rows = stmt.query_map(params![trait_name], |row| {
        Ok(ImplRecord {
            type_name: row.get::<_, String>(0)?,
            trait_name: row.get::<_, Option<String>>(1)?,
            file: PathBuf::from(row.get::<_, String>(2)?),
            start_line: row.get::<_, i64>(3)? as u32,
            start_col: row.get::<_, i64>(4)? as u32,
        })
    })?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Distinct file paths that define symbols in `module_path`, ordered by path.
pub fn files_for_module_path(conn: &Connection, module_path: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT f.path
         FROM symbols s JOIN files f ON s.file_id = f.id
         WHERE s.module_path = ?1
         ORDER BY f.path",
    )?;
    let rows = stmt.query_map(params![module_path], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// First file (by path order) that defines a symbol in `module_path`, if any.
pub fn first_file_for_module_path(conn: &Connection, module_path: &str) -> Result<Option<PathBuf>> {
    let files = files_for_module_path(conn, module_path)?;
    Ok(files.into_iter().next().map(PathBuf::from))
}

/// Reference names recorded in the file at `path`, ordered deterministically.
pub fn reference_names_in_file(conn: &Connection, path: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        r#"SELECT DISTINCT r.name
           FROM "references" r JOIN files f ON r.file_id = f.id
           WHERE f.path = ?1
           ORDER BY r.name"#,
    )?;
    let rows = stmt.query_map(params![path], |row| row.get::<_, String>(0))?;
    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::schema;
    use crate::graph::types::{
        FileNode, ImplRecord, Import, Reference, ReferenceKind, Symbol, SymbolKind,
    };
    use rusqlite::Connection;
    use std::path::PathBuf;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        schema::initialize(&conn).expect("init schema");
        conn
    }

    #[test]
    fn insert_and_find_definition_by_name() {
        let conn = setup();
        let file_id = insert_file(
            &conn,
            &FileNode { path: PathBuf::from("src/a.rs"), content_hash: "h".to_string() },
        )
        .unwrap();
        insert_symbols(
            &conn,
            file_id,
            &[Symbol {
                name: "AuthService".to_string(),
                kind: SymbolKind::Struct,
                file: PathBuf::new(),
                start_line: 10,
                start_col: 1,
                module_path: String::new(),
            }],
        )
        .unwrap();

        let defs = find_definition(&conn, "AuthService").unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "AuthService");
        assert_eq!(defs[0].kind, SymbolKind::Struct);
        assert_eq!(defs[0].file, PathBuf::from("src/a.rs"));
        assert_eq!(defs[0].start_line, 10);
        assert_eq!(defs[0].module_path, "");
    }

    #[test]
    fn insert_and_find_references_by_name() {
        let conn = setup();
        let file_id = insert_file(
            &conn,
            &FileNode { path: PathBuf::from("src/b.rs"), content_hash: "h".to_string() },
        )
        .unwrap();
        insert_references(
            &conn,
            file_id,
            &[Reference {
                name: "create_order".to_string(),
                file: PathBuf::new(),
                start_line: 5,
                start_col: 9,
                kind: ReferenceKind::Call,
                container: String::new(),
            }],
        )
        .unwrap();

        let refs = find_references(&conn, "create_order").unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].start_line, 5);
        assert_eq!(refs[0].file, PathBuf::from("src/b.rs"));
        assert_eq!(refs[0].kind, ReferenceKind::Call);
        assert_eq!(refs[0].container, "");
    }

    #[test]
    fn insert_symbols_and_references_round_trip_new_columns() {
        let conn = setup();
        let file_id = insert_file(
            &conn,
            &FileNode { path: PathBuf::from("src/c.rs"), content_hash: "h".to_string() },
        )
        .unwrap();
        insert_symbols(
            &conn,
            file_id,
            &[Symbol {
                name: "handler".to_string(),
                kind: SymbolKind::Function,
                file: PathBuf::new(),
                start_line: 3,
                start_col: 1,
                module_path: "crate::api".to_string(),
            }],
        )
        .unwrap();
        insert_references(
            &conn,
            file_id,
            &[Reference {
                name: "spawn".to_string(),
                file: PathBuf::new(),
                start_line: 4,
                start_col: 5,
                kind: ReferenceKind::Method,
                container: "handler".to_string(),
            }],
        )
        .unwrap();

        let defs = find_definition(&conn, "handler").unwrap();
        assert_eq!(defs[0].module_path, "crate::api");

        let refs = find_references(&conn, "spawn").unwrap();
        assert_eq!(refs[0].kind, ReferenceKind::Method);
        assert_eq!(refs[0].container, "handler");
    }

    #[test]
    fn insert_imports_persists_rows_with_nullable_alias() {
        let conn = setup();
        let file_id = insert_file(
            &conn,
            &FileNode { path: PathBuf::from("src/d.rs"), content_hash: "h".to_string() },
        )
        .unwrap();
        insert_imports(
            &conn,
            file_id,
            &[
                Import {
                    module_path: "std::collections::HashMap".to_string(),
                    alias: None,
                    file: PathBuf::new(),
                },
                Import {
                    module_path: "std::fmt::Result".to_string(),
                    alias: Some("FmtResult".to_string()),
                    file: PathBuf::new(),
                },
            ],
        )
        .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM imports WHERE file_id = ?1", params![file_id], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
        let null_aliases: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM imports WHERE alias IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(null_aliases, 1);
    }

    #[test]
    fn find_implementations_returns_trait_impls_ordered_excludes_inherent() {
        let conn = setup();
        let file_id = insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("src/lib.rs"),
                content_hash: "h".to_string(),
            },
        )
        .unwrap();
        insert_impls(
            &conn,
            file_id,
            &[
                ImplRecord {
                    type_name: "A".to_string(),
                    trait_name: Some("Storage".to_string()),
                    file: PathBuf::new(),
                    start_line: 10,
                    start_col: 1,
                },
                ImplRecord {
                    type_name: "B".to_string(),
                    trait_name: Some("Storage".to_string()),
                    file: PathBuf::new(),
                    start_line: 14,
                    start_col: 1,
                },
                ImplRecord {
                    type_name: "A".to_string(),
                    trait_name: None,
                    file: PathBuf::new(),
                    start_line: 18,
                    start_col: 1,
                },
            ],
        )
        .unwrap();

        let impls = find_implementations(&conn, "Storage").unwrap();
        assert_eq!(impls.len(), 2);
        assert_eq!(impls[0].type_name, "A");
        assert_eq!(impls[0].trait_name.as_deref(), Some("Storage"));
        assert_eq!(impls[0].file, PathBuf::from("src/lib.rs"));
        assert_eq!(impls[0].start_line, 10);
        assert_eq!(impls[1].type_name, "B");
        assert_eq!(impls[1].start_line, 14);
    }

    #[test]
    fn insert_impls_persists_rows_with_nullable_trait() {
        let conn = setup();
        let file_id = insert_file(
            &conn,
            &FileNode { path: PathBuf::from("src/e.rs"), content_hash: "h".to_string() },
        )
        .unwrap();
        insert_impls(
            &conn,
            file_id,
            &[
                ImplRecord {
                    type_name: "AuthService".to_string(),
                    trait_name: None,
                    file: PathBuf::new(),
                    start_line: 1,
                    start_col: 1,
                },
                ImplRecord {
                    type_name: "AuthService".to_string(),
                    trait_name: Some("Storage".to_string()),
                    file: PathBuf::new(),
                    start_line: 5,
                    start_col: 1,
                },
            ],
        )
        .unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM impls WHERE file_id = ?1", params![file_id], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
        let null_traits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM impls WHERE trait_name IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(null_traits, 1);
    }

    #[test]
    fn clear_file_rows_removes_imports_and_impls() {
        let conn = setup();
        let file_id = insert_file(
            &conn,
            &FileNode { path: PathBuf::from("src/f.rs"), content_hash: "h".to_string() },
        )
        .unwrap();
        insert_imports(
            &conn,
            file_id,
            &[Import {
                module_path: "std::io".to_string(),
                alias: None,
                file: PathBuf::new(),
            }],
        )
        .unwrap();
        insert_impls(
            &conn,
            file_id,
            &[ImplRecord {
                type_name: "T".to_string(),
                trait_name: None,
                file: PathBuf::new(),
                start_line: 1,
                start_col: 1,
            }],
        )
        .unwrap();

        clear_file_rows(&conn, file_id).unwrap();

        let imports: i64 = conn
            .query_row("SELECT COUNT(*) FROM imports", [], |r| r.get(0))
            .unwrap();
        let impls: i64 = conn
            .query_row("SELECT COUNT(*) FROM impls", [], |r| r.get(0))
            .unwrap();
        assert_eq!(imports, 0);
        assert_eq!(impls, 0);
    }

    #[test]
    fn reindexing_same_path_updates_hash_not_duplicates() {
        let conn = setup();
        let a = insert_file(&conn, &FileNode { path: PathBuf::from("src/a.rs"), content_hash: "h1".into() }).unwrap();
        let b = insert_file(&conn, &FileNode { path: PathBuf::from("src/a.rs"), content_hash: "h2".into() }).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn existing_hashes_returns_path_to_hash_map() {
        let conn = setup();
        insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("src/a.rs"),
                content_hash: "ha".into(),
            },
        )
        .unwrap();
        insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("src/b.rs"),
                content_hash: "hb".into(),
            },
        )
        .unwrap();

        let hashes = existing_hashes(&conn).unwrap();
        assert_eq!(hashes.len(), 2);
        assert_eq!(hashes.get("src/a.rs").map(String::as_str), Some("ha"));
        assert_eq!(hashes.get("src/b.rs").map(String::as_str), Some("hb"));
    }

    #[test]
    fn delete_file_and_rows_removes_file_and_dependents() {
        let conn = setup();
        let file_id = insert_file(
            &conn,
            &FileNode {
                path: PathBuf::from("src/gone.rs"),
                content_hash: "h".into(),
            },
        )
        .unwrap();
        insert_symbols(
            &conn,
            file_id,
            &[Symbol {
                name: "gone".to_string(),
                kind: SymbolKind::Function,
                file: PathBuf::new(),
                start_line: 1,
                start_col: 1,
                module_path: "crate".to_string(),
            }],
        )
        .unwrap();
        insert_imports(
            &conn,
            file_id,
            &[Import {
                module_path: "std::io".to_string(),
                alias: None,
                file: PathBuf::new(),
            }],
        )
        .unwrap();

        delete_file_and_rows(&conn, "src/gone.rs").unwrap();

        let files: i64 = conn
            .query_row("SELECT COUNT(*) FROM files", [], |r| r.get(0))
            .unwrap();
        let symbols: i64 = conn
            .query_row("SELECT COUNT(*) FROM symbols", [], |r| r.get(0))
            .unwrap();
        let imports: i64 = conn
            .query_row("SELECT COUNT(*) FROM imports", [], |r| r.get(0))
            .unwrap();
        assert_eq!(files, 0);
        assert_eq!(symbols, 0);
        assert_eq!(imports, 0);
    }
}
