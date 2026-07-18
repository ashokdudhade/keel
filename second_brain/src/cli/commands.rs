//! Execution logic behind each CLI subcommand.

use crate::db::{queries, schema};
use crate::error::{Result, SecondBrainError};
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

/// Index the repository at `path`. Returns the number of files indexed.
pub fn run_index(path: &Path) -> Result<usize> {
    let mut conn = open_db()?;
    index::index_repository(path, &mut conn)
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
