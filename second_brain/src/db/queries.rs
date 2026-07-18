//! Type-safe insert/select wrappers over the SQLite tables.

use crate::error::Result;
use crate::graph::types::{FileNode, Reference, Symbol, SymbolKind};
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

/// Delete all `symbols` and `references` rows for `file_id`.
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
    Ok(())
}

/// Insert all symbols for a file.
pub fn insert_symbols(conn: &Connection, file_id: i64, symbols: &[Symbol]) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT INTO symbols (file_id, name, kind, start_line, start_col) VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for s in symbols {
        stmt.execute(params![file_id, s.name, s.kind.as_db(), s.start_line as i64, s.start_col as i64])?;
    }
    Ok(())
}

/// Insert all references for a file.
pub fn insert_references(conn: &Connection, file_id: i64, references: &[Reference]) -> Result<()> {
    let mut stmt = conn.prepare(
        r#"INSERT INTO "references" (file_id, name, start_line, start_col) VALUES (?1, ?2, ?3, ?4)"#,
    )?;
    for r in references {
        stmt.execute(params![file_id, r.name, r.start_line as i64, r.start_col as i64])?;
    }
    Ok(())
}

/// Find all symbol definitions matching `name`, ordered deterministically.
pub fn find_definition(conn: &Connection, name: &str) -> Result<Vec<Symbol>> {
    let mut stmt = conn.prepare(
        "SELECT s.name, s.kind, f.path, s.start_line, s.start_col
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
        r#"SELECT r.name, f.path, r.start_line, r.start_col
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
    use crate::graph::types::{FileNode, Reference, Symbol, SymbolKind};
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
            }],
        )
        .unwrap();

        let defs = find_definition(&conn, "AuthService").unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].name, "AuthService");
        assert_eq!(defs[0].kind, SymbolKind::Struct);
        assert_eq!(defs[0].file, PathBuf::from("src/a.rs"));
        assert_eq!(defs[0].start_line, 10);
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
            &[Reference { name: "create_order".to_string(), file: PathBuf::new(), start_line: 5, start_col: 9 }],
        )
        .unwrap();

        let refs = find_references(&conn, "create_order").unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].start_line, 5);
        assert_eq!(refs[0].file, PathBuf::from("src/b.rs"));
    }

    #[test]
    fn reindexing_same_path_updates_hash_not_duplicates() {
        let conn = setup();
        let a = insert_file(&conn, &FileNode { path: PathBuf::from("src/a.rs"), content_hash: "h1".into() }).unwrap();
        let b = insert_file(&conn, &FileNode { path: PathBuf::from("src/a.rs"), content_hash: "h2".into() }).unwrap();
        assert_eq!(a, b);
    }
}
