//! SQLite schema definition and initialization.
//!
//! The schema is versioned via SQLite's `PRAGMA user_version`. `initialize`
//! acts as a migration runner: a fresh database (version 0) is created directly
//! at the latest version, while a v0.1 database (version 1) is upgraded in place
//! without losing data. The runner is idempotent.

use crate::error::Result;
use rusqlite::Connection;

/// The latest schema version this build understands.
const SCHEMA_VERSION: i64 = 2;

/// Create or migrate the schema to the latest version. Idempotent.
///
/// Reads `PRAGMA user_version` and:
/// - version 0 (fresh database): creates all v2 tables and indexes;
/// - version 1 (v0.1 database): adds the new columns/tables/indexes in place,
///   preserving existing data;
/// - version 2: no-op.
///
/// Finally stamps `user_version` to [`SCHEMA_VERSION`].
///
/// `references` is a reserved SQL keyword, so it is always quoted.
pub fn initialize(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;

    if version == 0 {
        create_v2(conn)?;
    } else if version == 1 {
        migrate_v1_to_v2(conn)?;
    }

    // PRAGMA does not accept bound parameters, and SCHEMA_VERSION is a trusted
    // integer constant, so formatting it into the statement is safe.
    conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
    Ok(())
}

/// Create the full v2 schema on a fresh database.
fn create_v2(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS files (
            id           INTEGER PRIMARY KEY,
            path         TEXT UNIQUE NOT NULL,
            content_hash TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS symbols (
            id          INTEGER PRIMARY KEY,
            file_id     INTEGER NOT NULL REFERENCES files(id),
            name        TEXT NOT NULL,
            kind        TEXT NOT NULL,
            start_line  INTEGER NOT NULL,
            start_col   INTEGER NOT NULL,
            module_path TEXT NOT NULL DEFAULT ''
        );
        CREATE TABLE IF NOT EXISTS "references" (
            id         INTEGER PRIMARY KEY,
            file_id    INTEGER NOT NULL REFERENCES files(id),
            name       TEXT NOT NULL,
            start_line INTEGER NOT NULL,
            start_col  INTEGER NOT NULL,
            kind       TEXT NOT NULL DEFAULT 'call',
            container  TEXT NOT NULL DEFAULT ''
        );
        CREATE TABLE IF NOT EXISTS imports (
            id          INTEGER PRIMARY KEY,
            file_id     INTEGER NOT NULL REFERENCES files(id),
            module_path TEXT NOT NULL,
            alias       TEXT
        );
        CREATE TABLE IF NOT EXISTS impls (
            id         INTEGER PRIMARY KEY,
            file_id    INTEGER NOT NULL REFERENCES files(id),
            type_name  TEXT NOT NULL,
            trait_name TEXT,
            start_line INTEGER NOT NULL,
            start_col  INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
        CREATE INDEX IF NOT EXISTS idx_symbols_module ON symbols(module_path, name);
        CREATE INDEX IF NOT EXISTS idx_references_name ON "references"(name);
        CREATE INDEX IF NOT EXISTS idx_imports_module ON imports(module_path);
        CREATE INDEX IF NOT EXISTS idx_impls_trait ON impls(trait_name);
        CREATE INDEX IF NOT EXISTS idx_impls_type ON impls(type_name);
        "#,
    )?;
    Ok(())
}

/// Upgrade an existing v0.1 (version 1) database to v2 in place.
fn migrate_v1_to_v2(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        ALTER TABLE symbols ADD COLUMN module_path TEXT NOT NULL DEFAULT '';
        ALTER TABLE "references" ADD COLUMN kind TEXT NOT NULL DEFAULT 'call';
        ALTER TABLE "references" ADD COLUMN container TEXT NOT NULL DEFAULT '';
        CREATE TABLE IF NOT EXISTS imports (
            id          INTEGER PRIMARY KEY,
            file_id     INTEGER NOT NULL REFERENCES files(id),
            module_path TEXT NOT NULL,
            alias       TEXT
        );
        CREATE TABLE IF NOT EXISTS impls (
            id         INTEGER PRIMARY KEY,
            file_id    INTEGER NOT NULL REFERENCES files(id),
            type_name  TEXT NOT NULL,
            trait_name TEXT,
            start_line INTEGER NOT NULL,
            start_col  INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_symbols_module ON symbols(module_path, name);
        CREATE INDEX IF NOT EXISTS idx_imports_module ON imports(module_path);
        CREATE INDEX IF NOT EXISTS idx_impls_trait ON impls(trait_name);
        CREATE INDEX IF NOT EXISTS idx_impls_type ON impls(type_name);
        "#,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_version(conn: &Connection) -> i64 {
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("read user_version")
    }

    fn table_exists(conn: &Connection, table: &str) -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get::<_, i64>(0),
        )
        .expect("query sqlite_master")
            > 0
    }

    fn table_has_column(conn: &Connection, table: &str, column: &str) -> bool {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info(\"{table}\")"))
            .expect("prepare table_info");
        let names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .expect("query table_info")
            .map(|r| r.expect("column name"))
            .collect();
        names.iter().any(|n| n == column)
    }

    // Minimal v0.1 (v1) schema used to simulate an existing on-disk database.
    const V1_SCHEMA: &str = r#"
        CREATE TABLE files (
            id           INTEGER PRIMARY KEY,
            path         TEXT UNIQUE NOT NULL,
            content_hash TEXT NOT NULL
        );
        CREATE TABLE symbols (
            id         INTEGER PRIMARY KEY,
            file_id    INTEGER NOT NULL REFERENCES files(id),
            name       TEXT NOT NULL,
            kind       TEXT NOT NULL,
            start_line INTEGER NOT NULL,
            start_col  INTEGER NOT NULL
        );
        CREATE TABLE "references" (
            id         INTEGER PRIMARY KEY,
            file_id    INTEGER NOT NULL REFERENCES files(id),
            name       TEXT NOT NULL,
            start_line INTEGER NOT NULL,
            start_col  INTEGER NOT NULL
        );
    "#;

    #[test]
    fn fresh_db_initializes_to_v2() {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        initialize(&conn).expect("init schema");

        assert_eq!(user_version(&conn), 2);
        assert!(table_has_column(&conn, "symbols", "module_path"));
        assert!(table_has_column(&conn, "references", "kind"));
        assert!(table_has_column(&conn, "references", "container"));
        assert!(table_exists(&conn, "imports"));
        assert!(table_exists(&conn, "impls"));
    }

    #[test]
    fn initialize_is_idempotent() {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        initialize(&conn).expect("init schema");
        initialize(&conn).expect("re-init schema");
        assert_eq!(user_version(&conn), 2);
    }

    #[test]
    fn v1_db_upgrades_to_v2_preserving_files() {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch(V1_SCHEMA).expect("create v1 schema");
        conn.execute_batch("PRAGMA user_version = 1;")
            .expect("set v1 version");
        conn.execute(
            "INSERT INTO files (path, content_hash) VALUES ('src/a.rs', 'h')",
            [],
        )
        .expect("seed files");

        initialize(&conn).expect("upgrade schema");

        assert_eq!(user_version(&conn), 2);
        assert!(table_has_column(&conn, "symbols", "module_path"));
        assert!(table_has_column(&conn, "references", "kind"));
        assert!(table_has_column(&conn, "references", "container"));
        assert!(table_exists(&conn, "imports"));
        assert!(table_exists(&conn, "impls"));

        let path: String = conn
            .query_row("SELECT path FROM files", [], |row| row.get(0))
            .expect("file preserved");
        assert_eq!(path, "src/a.rs");
    }
}
