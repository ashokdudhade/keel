//! SQLite storage layer: schema and type-safe queries.

pub mod queries;
pub mod schema;

use crate::error::Result;
use rusqlite::Connection;

/// Apply connection pragmas shared by CLI, API, and the library facade.
///
/// Sets `busy_timeout=5000` and attempts `journal_mode=WAL`. WAL failures are
/// ignored (in-memory databases may not support WAL).
pub fn configure_connection(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "busy_timeout", 5000)?;
    let _ = conn.pragma_update(None, "journal_mode", "WAL");
    Ok(())
}
