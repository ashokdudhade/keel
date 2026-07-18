//! Indexing orchestration: crawl, parse in parallel, then persist in one
//! transaction.

pub mod worker;

use crate::db::{queries, schema};
use crate::error::Result;
use crate::languages::Registry;
use rusqlite::Connection;
use std::path::Path;

/// Index every `.rs` file under `root` into `conn`. Returns the number of files
/// indexed.
pub fn index_repository(root: &Path, conn: &mut Connection) -> Result<usize> {
    schema::initialize(conn)?;
    let registry = Registry::with_defaults();
    let files = worker::collect_rust_files(root);
    let parsed = worker::parse_all(&files, &registry)?;

    let tx = conn.transaction()?;
    let mut count = 0usize;
    for pf in &parsed {
        let file_id = queries::insert_file(&tx, &pf.node)?;
        queries::clear_file_rows(&tx, file_id)?;
        queries::insert_symbols(&tx, file_id, &pf.symbols)?;
        queries::insert_references(&tx, file_id, &pf.references)?;
        count += 1;
    }
    tx.commit()?;
    Ok(count)
}
