//! SQLite storage layer: schema and type-safe queries.
//!
//! All connection construction flows through the factories below, so
//! `rusqlite` setup (pragmas, migrations) lives in exactly one place.
//! Callers hold a [`rusqlite::Connection`] handle afterwards — the SQL
//! itself stays centralized in [`queries`], and only this module (plus
//! [`schema`]) knows how a fresh handle is prepared.

pub mod queries;
pub mod schema;

use crate::error::Result;
use rusqlite::Connection;
use std::path::Path;

/// Open (or create) the database at `path`, apply shared pragmas, and run
/// pending migrations. This is the default factory for query paths.
pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    configure_connection(&conn)?;
    schema::initialize(&conn)?;
    Ok(conn)
}

/// In-memory variant of [`open`] (tests, ephemeral analysis).
pub fn open_in_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    configure_connection(&conn)?;
    schema::initialize(&conn)?;
    Ok(conn)
}

/// Open (or create) the database with pragmas but no migrations, for
/// callers that migrate through `index_repository` instead (daemon start,
/// CLI index/watch paths).
pub fn open_uninitialized(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)?;
    configure_connection(&conn)?;
    Ok(conn)
}

/// Open an existing database read-only, without pragmas or migrations:
/// inspection-only paths (e.g. `doctor`) must never create or migrate.
pub fn open_read_only(path: &Path) -> Result<Connection> {
    Ok(Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?)
}

/// Apply connection pragmas shared by CLI, API, and the library facade.
///
/// Sets `busy_timeout=5000` and attempts `journal_mode=WAL`. WAL failures are
/// ignored (in-memory databases may not support WAL).
pub fn configure_connection(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "busy_timeout", 5000)?;
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user_version(conn: &Connection) -> i64 {
        conn.query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap()
    }

    #[test]
    fn open_initializes_schema() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        let conn = open(&db).unwrap();
        assert!(user_version(&conn) > 0);
        // Core tables exist and are queryable.
        let tables: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name IN ('files', 'symbols', 'knowledge')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(tables, 3);
    }

    #[test]
    fn open_in_memory_initializes_schema() {
        let conn = open_in_memory().unwrap();
        assert!(user_version(&conn) > 0);
    }

    #[test]
    fn open_uninitialized_skips_migrations() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        let conn = open_uninitialized(&db).unwrap();
        assert_eq!(user_version(&conn), 0);
    }

    #[test]
    fn open_read_only_refuses_to_create() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.db");
        assert!(open_read_only(&missing).is_err());
        assert!(!missing.exists());
    }

    #[test]
    fn open_read_only_leaves_schema_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("index.db");
        // Create a version-0 database, then inspect it read-only.
        drop(open_uninitialized(&db).unwrap());
        let conn = open_read_only(&db).unwrap();
        assert_eq!(user_version(&conn), 0);
    }
}
