//! Execution logic behind each CLI subcommand.

use crate::db::{queries, schema};
use crate::error::{Result, SecondBrainError};
use crate::graph::resolve;
use crate::graph::types::{Reference, Symbol};
use crate::index;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

const DB_DIR: &str = ".secondbrain";
const DB_FILE: &str = "index.db";

fn db_path() -> PathBuf {
    Path::new(DB_DIR).join(DB_FILE)
}

/// Open (creating the directory if needed) the on-disk index database.
fn open_db() -> Result<Connection> {
    std::fs::create_dir_all(DB_DIR)
        .map_err(|source| SecondBrainError::Io { path: PathBuf::from(DB_DIR), source })?;
    let conn = Connection::open(db_path())?;
    Ok(conn)
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

/// Look up definitions by name.
pub fn run_definition(name: &str) -> Result<Vec<Symbol>> {
    let conn = open_db()?;
    schema::initialize(&conn)?;
    queries::find_definition(&conn, name)
}

/// Look up references by name (also used for `callers` in v0.1).
pub fn run_references(name: &str) -> Result<Vec<Reference>> {
    let conn = open_db()?;
    schema::initialize(&conn)?;
    queries::find_references(&conn, name)
}

/// Look up callers of `name` with import-aware precision when a unique
/// definition module can be determined; otherwise falls back to all sites.
pub fn run_callers(name: &str) -> Result<Vec<Reference>> {
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
