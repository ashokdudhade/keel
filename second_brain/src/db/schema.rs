//! SQLite schema definition and initialization.
//!
//! The schema is versioned via SQLite's `PRAGMA user_version`. `initialize`
//! acts as a migration runner: a fresh database (version 0) is created directly
//! at the latest version, while an existing v0.1 database is upgraded in place
//! without losing data. The runner is idempotent.
//!
//! v0.1 never stamped a version, so a real on-disk v0.1 database reports
//! `user_version = 0` while already holding populated tables. Version 0 is
//! therefore disambiguated by probing for the `files` table: if it exists the
//! database is a legacy v0.1 that must be upgraded, otherwise it is truly fresh.

use crate::error::{Result, SecondBrainError};
use rusqlite::Connection;

/// The latest schema version this build understands.
const SCHEMA_VERSION: i64 = 2;

/// Create or migrate the schema to the latest version. Idempotent.
///
/// Reads `PRAGMA user_version` and:
/// - version 0 with no `files` table (truly fresh): creates all v2 tables and
///   indexes;
/// - version 0 with an existing `files` table (unstamped legacy v0.1 database):
///   runs the v0.1 → v2 upgrade in place, preserving existing data;
/// - version 1 (stamped v0.1 database): runs the same v0.1 → v2 upgrade;
/// - version 2: no-op;
/// - version > [`SCHEMA_VERSION`]: returns [`SecondBrainError::UnsupportedSchema`]
///   without stamping.
///
/// The whole migration runs inside a single transaction so a partial failure
/// cannot leave a half-migrated database, and `user_version` is stamped to
/// [`SCHEMA_VERSION`] at the end (only when the starting version is supported).
///
/// `references` is a reserved SQL keyword, so it is always quoted.
pub fn initialize(conn: &Connection) -> Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;

    conn.execute_batch("BEGIN;")?;
    if let Err(e) = migrate(conn, version) {
        // Best-effort rollback; surface the original migration error.
        let _ = conn.execute_batch("ROLLBACK;");
        return Err(e);
    }
    conn.execute_batch("COMMIT;")?;
    Ok(())
}

/// Apply the migration steps for the given starting `version` and stamp the
/// schema version. Must be called inside a transaction.
fn migrate(conn: &Connection, version: i64) -> Result<()> {
    if version > SCHEMA_VERSION {
        return Err(SecondBrainError::UnsupportedSchema {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }

    match version {
        // Version 0 is ambiguous: a truly fresh database, or an unstamped
        // legacy v0.1 database that already contains data. Probe for `files`.
        0 => {
            if table_exists(conn, "files")? {
                upgrade_to_v2(conn)?;
            } else {
                create_v2(conn)?;
            }
        }
        1 => upgrade_to_v2(conn)?,
        _ => {}
    }

    // PRAGMA does not accept bound parameters, and SCHEMA_VERSION is a trusted
    // integer constant, so formatting it into the statement is safe.
    conn.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
    Ok(())
}

/// Return whether a table with the given `name` exists.
fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Return whether `table` already has a column named `column`.
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    // `table` is a trusted internal identifier; quote it to tolerate reserved
    // words such as `references`. PRAGMA table_info cannot bind parameters.
    let mut stmt = conn.prepare(&format!("PRAGMA table_info(\"{table}\")"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let name: String = row.get(1)?;
        if name == column {
            return Ok(true);
        }
    }
    Ok(false)
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

/// Upgrade an existing v0.1 database (stamped version 1, or an unstamped legacy
/// database still reporting version 0) to v2 in place, preserving data.
///
/// Each `ALTER TABLE ... ADD COLUMN` is guarded by a `PRAGMA table_info` check
/// so the upgrade is safe to re-run: SQLite has no `ADD COLUMN IF NOT EXISTS`
/// and would otherwise error with "duplicate column name". New tables and
/// indexes use `IF NOT EXISTS` and are already idempotent.
fn upgrade_to_v2(conn: &Connection) -> Result<()> {
    if !column_exists(conn, "symbols", "module_path")? {
        conn.execute_batch("ALTER TABLE symbols ADD COLUMN module_path TEXT NOT NULL DEFAULT '';")?;
    }
    if !column_exists(conn, "references", "kind")? {
        conn.execute_batch(
            "ALTER TABLE \"references\" ADD COLUMN kind TEXT NOT NULL DEFAULT 'call';",
        )?;
    }
    if !column_exists(conn, "references", "container")? {
        conn.execute_batch(
            "ALTER TABLE \"references\" ADD COLUMN container TEXT NOT NULL DEFAULT '';",
        )?;
    }

    conn.execute_batch(
        r#"
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
    fn legacy_v0_unstamped_db_upgrades_to_v2_preserving_files() {
        // Simulate a REAL on-disk v0.1 database: the v0.1 tables exist and hold
        // data, but `user_version` was never stamped, so it still reports 0.
        let conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch(V1_SCHEMA).expect("create v0.1 schema");
        conn.execute(
            "INSERT INTO files (path, content_hash) VALUES ('src/legacy.rs', 'h0')",
            [],
        )
        .expect("seed files");
        // Deliberately DO NOT set user_version: it stays 0 like a real v0.1 db.
        assert_eq!(user_version(&conn), 0);

        initialize(&conn).expect("migrate legacy v0 db");

        assert_eq!(user_version(&conn), 2);
        assert!(table_has_column(&conn, "symbols", "module_path"));
        assert!(table_has_column(&conn, "references", "kind"));
        assert!(table_has_column(&conn, "references", "container"));
        assert!(table_exists(&conn, "imports"));
        assert!(table_exists(&conn, "impls"));

        let path: String = conn
            .query_row("SELECT path FROM files", [], |row| row.get(0))
            .expect("file preserved");
        assert_eq!(path, "src/legacy.rs");

        // Re-running must stay green (idempotent) on a migrated legacy db.
        initialize(&conn).expect("re-init migrated legacy db");
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

    #[test]
    fn newer_schema_version_is_rejected() {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch("PRAGMA user_version = 99;")
            .expect("stamp future version");

        let err = initialize(&conn).expect_err("must reject newer schema");
        match err {
            crate::error::SecondBrainError::UnsupportedSchema { found, supported } => {
                assert_eq!(found, 99);
                assert_eq!(supported, SCHEMA_VERSION);
            }
            other => panic!("unexpected error: {other}"),
        }
        // Must not stamp down to SCHEMA_VERSION.
        assert_eq!(user_version(&conn), 99);
    }
}
