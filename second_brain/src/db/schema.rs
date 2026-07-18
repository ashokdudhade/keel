//! SQLite schema definition and initialization.

use crate::error::Result;
use rusqlite::Connection;

/// Create all tables and indexes if they do not already exist. Idempotent.
///
/// `references` is a reserved SQL keyword, so it is always quoted.
pub fn initialize(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS files (
            id           INTEGER PRIMARY KEY,
            path         TEXT UNIQUE NOT NULL,
            content_hash TEXT NOT NULL
        );
        CREATE TABLE IF NOT EXISTS symbols (
            id         INTEGER PRIMARY KEY,
            file_id    INTEGER NOT NULL REFERENCES files(id),
            name       TEXT NOT NULL,
            kind       TEXT NOT NULL,
            start_line INTEGER NOT NULL,
            start_col  INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS "references" (
            id         INTEGER PRIMARY KEY,
            file_id    INTEGER NOT NULL REFERENCES files(id),
            name       TEXT NOT NULL,
            start_line INTEGER NOT NULL,
            start_col  INTEGER NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_symbols_name ON symbols(name);
        CREATE INDEX IF NOT EXISTS idx_references_name ON "references"(name);
        "#,
    )?;
    Ok(())
}
