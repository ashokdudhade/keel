//! Execution logic behind each CLI subcommand.

use crate::api;
use crate::db::{queries, schema};
use crate::error::{Result, SecondBrainError};
use crate::graph::deps::{self, Dependency};
use crate::graph::impact;
use crate::graph::resolve;
use crate::graph::types::{ImplRecord, Reference, Symbol};
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

/// Look up trait implementations by trait name.
pub fn run_implementations(trait_name: &str) -> Result<Vec<ImplRecord>> {
    let conn = open_db()?;
    schema::initialize(&conn)?;
    queries::find_implementations(&conn, trait_name)
}

/// Look up modules/files that `name` (module path or symbol) depends on.
pub fn run_dependencies(name: &str) -> Result<Vec<Dependency>> {
    let conn = open_db()?;
    schema::initialize(&conn)?;
    deps::find_dependencies(&conn, name)
}

/// Look up symbols transitively impacted by changing `name`.
pub fn run_impact(name: &str) -> Result<Vec<Symbol>> {
    let conn = open_db()?;
    schema::initialize(&conn)?;
    impact::find_impact(&conn, name)
}

/// Serve the JSON API on `127.0.0.1:{port}` using the on-disk index.
pub fn run_serve(port: u16) -> Result<()> {
    // Ensure the DB directory exists and schema is ready before binding.
    let conn = open_db()?;
    schema::initialize(&conn)?;
    drop(conn);
    let addr = format!("127.0.0.1:{port}");
    eprintln!("Serving SecondBrain JSON API on http://{addr}");
    api::serve(&addr, &db_path())
}
