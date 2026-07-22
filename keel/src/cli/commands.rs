//! Execution logic behind each CLI subcommand.

use crate::api;
use crate::db::{queries, schema};
use crate::error::{Result, KeelError};
use crate::graph::deps::{self, Dependency};
use crate::graph::impact;
use crate::graph::resolve;
use crate::graph::types::{ImplRecord, Reference, Symbol};
use crate::index::{self, IndexStats};
use crate::mcp;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

const DB_DIR: &str = ".keel";
const DB_FILE: &str = "index.db";

fn db_path() -> PathBuf {
    Path::new(DB_DIR).join(DB_FILE)
}

/// Open (creating the directory if needed) the on-disk index database.
fn open_db() -> Result<Connection> {
    std::fs::create_dir_all(DB_DIR)
        .map_err(|source| KeelError::Io {
            path: PathBuf::from(DB_DIR),
            source,
        })?;
    let conn = Connection::open(db_path())?;
    crate::db::configure_connection(&conn)?;
    Ok(conn)
}

/// Run a fast incremental index of `root` into the project DB.
///
/// Unchanged files are hash-skipped. Emits a one-line stderr note only when
/// something actually changed or failed.
pub fn ensure_index(root: &Path) -> Result<IndexStats> {
    let mut conn = open_db()?;
    let stats = index::index_repository(root, &mut conn)?;
    if stats.indexed + stats.removed + stats.errors > 0 {
        eprintln!(
            "keel: auto-indexed {} file(s) (skipped {}, removed {}, errors {}).",
            stats.indexed, stats.skipped, stats.removed, stats.errors
        );
    }
    Ok(stats)
}

fn maybe_ensure_index(auto_index: bool) -> Result<()> {
    if auto_index {
        ensure_index(Path::new("."))?;
    }
    Ok(())
}

/// Index the repository at `path`. Returns incremental indexing stats.
pub fn run_index(path: &Path) -> Result<index::IndexStats> {
    let mut conn = open_db()?;
    index::index_repository(path, &mut conn)
}

/// Watch the repository at `path` and re-index on changes until interrupted.
pub fn run_watch(path: &Path) -> Result<()> {
    let mut conn = open_db()?;
    index::watch::watch_repository(path, &mut conn)
}

/// Register `path` with the global daemon (indexes + watches the project).
pub fn run_start(path: &Path) -> Result<()> {
    crate::daemon::client_start_project(path)
}

/// Unregister the current project from the global daemon.
pub fn run_stop() -> Result<()> {
    crate::daemon::client_stop_project(Path::new("."))
}

/// Print global daemon + current project watch status.
pub fn run_status() -> Result<()> {
    crate::daemon::client_status(Path::new("."))
}

/// Run the global daemon (brew services).
pub fn run_daemon(port: u16) -> Result<()> {
    crate::daemon::run_daemon(port)
}

/// Look up definitions by name.
pub fn run_definition(name: &str, auto_index: bool) -> Result<Vec<Symbol>> {
    maybe_ensure_index(auto_index)?;
    let conn = open_db()?;
    schema::initialize(&conn)?;
    queries::find_definition(&conn, name)
}

/// Look up references by name (also used for `callers` in v0.1).
pub fn run_references(name: &str, auto_index: bool) -> Result<Vec<Reference>> {
    maybe_ensure_index(auto_index)?;
    let conn = open_db()?;
    schema::initialize(&conn)?;
    queries::find_references(&conn, name)
}

/// Look up callers of `name` with import-aware precision when a unique
/// definition module can be determined; otherwise falls back to all sites.
pub fn run_callers(name: &str, auto_index: bool) -> Result<Vec<Reference>> {
    maybe_ensure_index(auto_index)?;
    let conn = open_db()?;
    schema::initialize(&conn)?;
    let defs = queries::find_definition(&conn, name)?;
    let target_module = unique_module(&defs);
    resolve::find_callers(&conn, name, target_module.as_deref())
}

/// When every definition shares one `module_path`, return it for precise
/// caller filtering; otherwise `None` (name-based fallback).
fn unique_module(defs: &[Symbol]) -> Option<String> {
    let first = defs.first()?.module_path.clone();
    if defs.iter().all(|d| d.module_path == first) {
        Some(first)
    } else {
        None
    }
}

/// Look up trait implementations by trait name.
pub fn run_implementations(trait_name: &str, auto_index: bool) -> Result<Vec<ImplRecord>> {
    maybe_ensure_index(auto_index)?;
    let conn = open_db()?;
    schema::initialize(&conn)?;
    queries::find_implementations(&conn, trait_name)
}

/// Look up modules/files that `name` (module path or symbol) depends on.
pub fn run_dependencies(name: &str, auto_index: bool) -> Result<Vec<Dependency>> {
    maybe_ensure_index(auto_index)?;
    let conn = open_db()?;
    schema::initialize(&conn)?;
    deps::find_dependencies(&conn, name)
}

/// Look up symbols transitively impacted by changing `name`.
pub fn run_impact(name: &str, auto_index: bool) -> Result<Vec<Symbol>> {
    maybe_ensure_index(auto_index)?;
    let conn = open_db()?;
    schema::initialize(&conn)?;
    impact::find_impact(&conn, name)
}

/// Serve the JSON API on `127.0.0.1:{port}` using the on-disk index.
pub fn run_serve(port: u16, auto_index: bool) -> Result<()> {
    if auto_index {
        ensure_index(Path::new("."))?;
    } else {
        let conn = open_db()?;
        schema::initialize(&conn)?;
        drop(conn);
    }
    let addr = format!("127.0.0.1:{port}");
    eprintln!("Serving Keel JSON API on http://{addr}");
    api::serve(&addr, &db_path(), auto_index)
}

/// Serve the MCP stdio server using `.keel/index.db` under CWD.
pub fn run_mcp(auto_index: bool) -> Result<()> {
    std::fs::create_dir_all(DB_DIR).map_err(|source| KeelError::Io {
        path: PathBuf::from(DB_DIR),
        source,
    })?;
    eprintln!("Serving Keel MCP on stdio (db={})", db_path().display());
    mcp::serve(&db_path(), auto_index)
}

/// Project root that owns a `.keel/index.db` path.
pub fn index_root_from_db(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .and_then(|keel_dir| keel_dir.parent())
        .map(|p| {
            if p.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                p.to_path_buf()
            }
        })
        .unwrap_or_else(|| PathBuf::from("."))
}
