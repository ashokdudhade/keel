//! Type-safe insert/select wrappers over the SQLite tables.

use crate::error::Result;
use crate::graph::types::{
    FileNode, ImplRecord, Import, Reference, ReferenceKind, Symbol, SymbolKind,
};
use rusqlite::{params, Connection};
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
}
