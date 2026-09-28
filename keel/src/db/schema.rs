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

use crate::error::{Result, KeelError};
use rusqlite::Connection;

/// The latest schema version this build understands.
const SCHEMA_VERSION: i64 = 5;

/// Content-format version stamped into each index by the writer.
///
/// Unlike `SCHEMA_VERSION` (table shape), this tracks extraction semantics
/// (module identity, symbol/reference rules). Bump it whenever indexing output
/// changes meaning, so old indexes rebuild instead of serving stale answers.
/// Unstamped (legacy) indexes read back as 0 and always rebuild.
pub const INDEX_FORMAT_VERSION: i64 = 2;

/// Create or migrate the schema to the latest version. Idempotent.
///
/// Reads `PRAGMA user_version` and:
/// - version 0 with no `files` table (truly fresh): creates all v5 tables and
///   indexes, stamped current (nothing stale to rebuild);
/// - version 0 with an existing `files` table (unstamped legacy v0.1 database):
///   runs the v0.1 → v5 upgrade in place, preserving existing data; content is
///   stamped format 0 so the next index pass rebuilds it;
/// - version 1 (stamped v0.1 database): runs the same chained upgrade;
/// - version 2: adds the writer stamp (format 0, forcing one rebuild), then v5;
/// - version 3: adds the reference qualifier column, then forces one rebuild;
/// - version 4: adds `file_id` indexes (no rebuild: content is unchanged);
/// - version 5: no-op;
/// - version > `SCHEMA_VERSION`: returns [`KeelError::UnsupportedSchema`]
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
        return Err(KeelError::UnsupportedSchema {
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
                upgrade_to_v3(conn)?;
                upgrade_to_v4(conn)?;
                upgrade_to_v5(conn)?;
            } else {
                create_v5(conn)?;
            }
        }
        1 => {
            upgrade_to_v2(conn)?;
            upgrade_to_v3(conn)?;
            upgrade_to_v4(conn)?;
            upgrade_to_v5(conn)?;
        }
        2 => {
            upgrade_to_v3(conn)?;
            upgrade_to_v4(conn)?;
            upgrade_to_v5(conn)?;
        }
        3 => {
            upgrade_to_v4(conn)?;
            upgrade_to_v5(conn)?;
        }
        4 => upgrade_to_v5(conn)?,
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

/// Read the writer's content-format stamp: `INDEX_FORMAT_VERSION` when current,
/// 0 when unstamped, legacy, or unreadable (all rebuild-safe directions).
pub fn index_format_version(conn: &Connection) -> i64 {
    if !table_exists(conn, "meta").unwrap_or(false) {
        return 0;
    }
    conn.query_row(
        "SELECT value FROM meta WHERE key = 'index_format'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .and_then(|v| v.parse::<i64>().ok())
    .unwrap_or(0)
}

/// Stamp the writer's content format (and writer version, for bug reports).
pub fn stamp_index_format(conn: &Connection, version: i64) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('index_format', ?1)",
        [version.to_string()],
    )?;
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('keel_version', ?1)",
        [env!("CARGO_PKG_VERSION")],
    )?;
    Ok(())
}

/// Record a completed index pass (unix seconds) for health displays.
pub fn stamp_last_indexed(conn: &Connection) -> Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES ('last_indexed', ?1)",
        [now.to_string()],
    )?;
    Ok(())
}

/// Writer Keel version stamp, when present.
pub fn writer_version(conn: &Connection) -> Option<String> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = 'keel_version'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
}

/// Last completed index pass (unix seconds), 0 when never indexed.
pub fn last_indexed(conn: &Connection) -> u64 {
    conn.query_row(
        "SELECT value FROM meta WHERE key = 'last_indexed'",
        [],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .and_then(|v| v.parse::<u64>().ok())
    .unwrap_or(0)
}

/// Create the full v4 schema on a fresh database.
fn create_v4(conn: &Connection) -> Result<()> {
    create_v2(conn)?;
    // v4 adds the qualifier column to "references" at creation time.
    conn.execute_batch(
        r#"
        ALTER TABLE "references" ADD COLUMN qualifier TEXT NOT NULL DEFAULT '';
        "#,
    )?;
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        "#,
    )?;
    // Fresh databases hold no content, so stamping current is truthful.
    stamp_index_format(conn, INDEX_FORMAT_VERSION)?;
    Ok(())
}

/// Create the full v5 schema on a fresh database: v4 tables plus the
/// `file_id` indexes (every file-scoped query and incremental delete
/// filters on `file_id`; without them large repos full-scan per file).
fn create_v5(conn: &Connection) -> Result<()> {
    create_v4(conn)?;
    create_file_id_indexes(conn)?;
    Ok(())
}

/// `file_id` indexes for the four row tables. Idempotent (`IF NOT
/// EXISTS`), so both fresh creation and the v4 → v5 upgrade share it.
/// Each index is guarded by a table-existence check: `IF NOT EXISTS`
/// does not cover a missing table, and partial legacy databases must
/// still migrate instead of bricking.
fn create_file_id_indexes(conn: &Connection) -> Result<()> {
    for (table, index) in [
        ("symbols", "idx_symbols_file"),
        ("references", "idx_references_file"),
        ("imports", "idx_imports_file"),
        ("impls", "idx_impls_file"),
    ] {
        if !table_exists(conn, table)? {
            continue;
        }
        // Table and index names are trusted constants, so formatting
        // them into the statement is safe.
        conn.execute_batch(&format!(
            "CREATE INDEX IF NOT EXISTS {index} ON \"{table}\"(file_id);"
        ))?;
    }
    Ok(())
}

/// Upgrade a v4 database to v5: add the `file_id` indexes. Content
/// semantics are unchanged, so no rebuild is forced — existing rows
/// stay valid and immediately benefit.
fn upgrade_to_v5(conn: &Connection) -> Result<()> {
    create_file_id_indexes(conn)?;
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

/// Upgrade a v3 database to v4: add the reference qualifier column and mark
/// content format 0 (unknown writer), forcing one full rebuild on the next
/// index pass so qualifiers get populated. Indexed rows are preserved; the
/// rebuild replaces them, it does not strand the user on an error.
fn upgrade_to_v4(conn: &Connection) -> Result<()> {
    if !column_exists(conn, "references", "qualifier")? {
        conn.execute_batch(
            r#"ALTER TABLE "references" ADD COLUMN qualifier TEXT NOT NULL DEFAULT '';"#,
        )?;
    }
    stamp_index_format(conn, 0)?;
    Ok(())
}

/// Upgrade a v2 database to v3: add the writer-stamp table and mark content
/// format 0 (unknown writer), forcing one full rebuild on the next index pass.
/// Indexed rows are preserved; the rebuild replaces them, it does not strand
/// the user on an error.
fn upgrade_to_v3(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS meta (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        "#,
    )?;
    stamp_index_format(conn, 0)?;
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

    fn index_exists(conn: &Connection, name: &str) -> bool {
        conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
            [name],
            |row| row.get::<_, i64>(0),
        )
        .expect("query sqlite_master")
            > 0
    }

    #[test]
    fn fresh_db_initializes_to_v5() {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        initialize(&conn).expect("init schema");

        assert_eq!(user_version(&conn), 5);
        assert!(table_has_column(&conn, "symbols", "module_path"));
        assert!(table_has_column(&conn, "references", "kind"));
        assert!(table_has_column(&conn, "references", "container"));
        assert!(table_has_column(&conn, "references", "qualifier"));
        assert!(table_exists(&conn, "imports"));
        assert!(table_exists(&conn, "impls"));
        assert!(table_exists(&conn, "meta"));
        assert!(index_exists(&conn, "idx_symbols_file"));
        assert!(index_exists(&conn, "idx_references_file"));
        assert!(index_exists(&conn, "idx_imports_file"));
        assert!(index_exists(&conn, "idx_impls_file"));
        assert_eq!(index_format_version(&conn), INDEX_FORMAT_VERSION);
    }

    #[test]
    fn initialize_is_idempotent() {
        let conn = Connection::open_in_memory().expect("open in-memory db");
        initialize(&conn).expect("init schema");
        initialize(&conn).expect("re-init schema");
        assert_eq!(user_version(&conn), 5);
    }

    #[test]
    fn legacy_v0_unstamped_db_upgrades_to_v5_preserving_files() {
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

        assert_eq!(user_version(&conn), 5);
        assert!(table_has_column(&conn, "symbols", "module_path"));
        assert!(table_has_column(&conn, "references", "kind"));
        assert!(table_has_column(&conn, "references", "container"));
        assert!(table_has_column(&conn, "references", "qualifier"));
        assert!(table_exists(&conn, "imports"));
        assert!(table_exists(&conn, "impls"));

        let path: String = conn
            .query_row("SELECT path FROM files", [], |row| row.get(0))
            .expect("file preserved");
        assert_eq!(path, "src/legacy.rs");
        // Unknown writer: content must rebuild on the next index pass.
        assert_eq!(index_format_version(&conn), 0);

        // Re-running must stay green (idempotent) on a migrated legacy db.
        initialize(&conn).expect("re-init migrated legacy db");
        assert_eq!(user_version(&conn), 5);
    }

    #[test]
    fn v1_db_upgrades_to_v5_preserving_files() {
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

        assert_eq!(user_version(&conn), 5);
        assert!(table_has_column(&conn, "symbols", "module_path"));
        assert!(table_has_column(&conn, "references", "kind"));
        assert!(table_has_column(&conn, "references", "container"));
        assert!(table_has_column(&conn, "references", "qualifier"));
        assert!(table_exists(&conn, "imports"));
        assert!(table_exists(&conn, "impls"));

        let path: String = conn
            .query_row("SELECT path FROM files", [], |row| row.get(0))
            .expect("file preserved");
        assert_eq!(path, "src/a.rs");
        assert_eq!(index_format_version(&conn), 0);
    }

    #[test]
    fn v2_db_upgrades_to_v5_marking_content_stale() {
        // Simulate a v2 database: stamped 2 with content rows present.
        let conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch(
            r#"
            CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT UNIQUE NOT NULL, content_hash TEXT NOT NULL);
            CREATE TABLE "references" (id INTEGER PRIMARY KEY, file_id INTEGER NOT NULL REFERENCES files(id), name TEXT NOT NULL, start_line INTEGER NOT NULL, start_col INTEGER NOT NULL, kind TEXT NOT NULL DEFAULT 'call', container TEXT NOT NULL DEFAULT '');
            "#,
        )
        .expect("create v2 tables");
        conn.execute(
            "INSERT INTO files (path, content_hash) VALUES ('src/a.rs', 'h')",
            [],
        )
        .expect("seed files");
        conn.execute_batch("PRAGMA user_version = 2;")
            .expect("set v2 version");

        initialize(&conn).expect("upgrade schema");

        assert_eq!(user_version(&conn), 5);
        assert!(table_exists(&conn, "meta"));
        assert_eq!(index_format_version(&conn), 0);
        let path: String = conn
            .query_row("SELECT path FROM files", [], |row| row.get(0))
            .expect("file preserved");
        assert_eq!(path, "src/a.rs");
    }

    #[test]
    fn v3_db_upgrades_to_v5_adding_qualifier() {
        // Simulate a v3 database: full v3 tables stamped 3 with a reference row.
        let conn = Connection::open_in_memory().expect("open in-memory db");
        conn.execute_batch(
            r#"
            CREATE TABLE files (id INTEGER PRIMARY KEY, path TEXT UNIQUE NOT NULL, content_hash TEXT NOT NULL);
            CREATE TABLE "references" (id INTEGER PRIMARY KEY, file_id INTEGER NOT NULL REFERENCES files(id), name TEXT NOT NULL, start_line INTEGER NOT NULL, start_col INTEGER NOT NULL, kind TEXT NOT NULL DEFAULT 'call', container TEXT NOT NULL DEFAULT '');
            CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
            "#,
        )
        .expect("create v3 tables");
        conn.execute(
            "INSERT INTO files (path, content_hash) VALUES ('src/a.rs', 'h')",
            [],
        )
        .expect("seed files");
        conn.execute(
            "INSERT INTO \"references\" (file_id, name, start_line, start_col) VALUES (1, 'serve', 9, 10)",
            [],
        )
        .expect("seed references");
        conn.execute_batch("PRAGMA user_version = 3;")
            .expect("set v3 version");

        initialize(&conn).expect("upgrade schema");

        assert_eq!(user_version(&conn), 5);
        assert!(table_has_column(&conn, "references", "qualifier"));
        // Legacy rows default to the empty (unattributable) qualifier.
        let qualifier: String = conn
            .query_row("SELECT qualifier FROM \"references\"", [], |row| row.get(0))
            .expect("read qualifier");
        assert_eq!(qualifier, "");
        // Content rebuilds on the next index pass to populate qualifiers.
        assert_eq!(index_format_version(&conn), 0);

        initialize(&conn).expect("re-init stays green");
        assert_eq!(user_version(&conn), 5);
    }

    #[test]
    fn v4_db_upgrades_to_v5_adding_file_indexes_without_rebuild() {
        // Simulate a v4 database: current tables stamped 4 with content.
        let conn = Connection::open_in_memory().expect("open in-memory db");
        initialize(&conn).expect("create v5 schema");
        conn.execute_batch("PRAGMA user_version = 4;")
            .expect("downgrade stamp to v4");
        // A real v4 database has no file_id indexes yet.
        conn.execute_batch(
            "DROP INDEX idx_symbols_file; DROP INDEX idx_references_file; DROP INDEX idx_imports_file; DROP INDEX idx_impls_file;",
        )
        .expect("drop v5 indexes");
        conn.execute(
            "INSERT INTO files (path, content_hash) VALUES ('src/a.rs', 'h')",
            [],
        )
        .expect("seed files");

        initialize(&conn).expect("upgrade schema");

        assert_eq!(user_version(&conn), 5);
        assert!(index_exists(&conn, "idx_symbols_file"));
        assert!(index_exists(&conn, "idx_references_file"));
        assert!(index_exists(&conn, "idx_imports_file"));
        assert!(index_exists(&conn, "idx_impls_file"));
        // Index-only migration: content stays valid, no rebuild forced.
        assert_eq!(index_format_version(&conn), INDEX_FORMAT_VERSION);
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
            crate::error::KeelError::UnsupportedSchema { found, supported } => {
                assert_eq!(found, 99);
                assert_eq!(supported, SCHEMA_VERSION);
            }
            other => panic!("unexpected error: {other}"),
        }
        // Must not stamp down to SCHEMA_VERSION.
        assert_eq!(user_version(&conn), 99);
    }
}
