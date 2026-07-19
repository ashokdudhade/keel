//! Indexing orchestration: crawl, parse in parallel, then persist in one
//! transaction.

pub mod watch;
pub mod worker;

use crate::db::{queries, schema};
use crate::error::{Result, SecondBrainError};
use crate::languages::Registry;
use rayon::prelude::*;
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// Counts of files processed by an incremental indexing pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IndexStats {
    /// Files that were parsed and written (new or content-changed).
    pub indexed: usize,
    /// Files whose content hash matched the existing index and were left alone.
    pub skipped: usize,
    /// Files present in the DB but no longer on disk, whose rows were deleted.
    pub removed: usize,
}

/// Index every registered-language source file under `root` into `conn`.
///
/// Incremental: hashes candidate files first, skips unchanged paths, parses and
/// persists new/changed files, and deletes DB rows for files gone from disk.
pub fn index_repository(root: &Path, conn: &mut Connection) -> Result<IndexStats> {
    schema::initialize(conn)?;
    let registry = Registry::with_defaults();
    let files = worker::collect_source_files(root, &registry);
    let existing = queries::existing_hashes(conn)?;

    // Hash every candidate in parallel so skip/parse decisions are cheap.
    let hashed: Vec<(PathBuf, String)> = files
        .par_iter()
        .map(|path| hash_file(path).map(|hash| (path.clone(), hash)))
        .collect::<Result<Vec<_>>>()?;

    let mut to_parse = Vec::new();
    let mut skipped = 0usize;
    let mut on_disk: HashSet<String> = HashSet::with_capacity(hashed.len());

    for (path, hash) in hashed {
        let key = path.to_string_lossy().into_owned();
        on_disk.insert(key.clone());
        match existing.get(&key) {
            Some(prev) if prev == &hash => skipped += 1,
            _ => to_parse.push(path),
        }
    }

    let parsed = worker::parse_all(&to_parse, &registry)?;

    let mut removed = 0usize;
    let tx = conn.transaction()?;
    for pf in &parsed {
        let file_id = queries::insert_file(&tx, &pf.node)?;
        queries::clear_file_rows(&tx, file_id)?;
        queries::insert_symbols(&tx, file_id, &pf.symbols)?;
        queries::insert_references(&tx, file_id, &pf.references)?;
        queries::insert_imports(&tx, file_id, &pf.imports)?;
        queries::insert_impls(&tx, file_id, &pf.impls)?;
    }
    for path in existing.keys() {
        if !on_disk.contains(path) {
            queries::delete_file_and_rows(&tx, path)?;
            removed += 1;
        }
    }
    tx.commit()?;

    Ok(IndexStats {
        indexed: parsed.len(),
        skipped,
        removed,
    })
}

/// SHA-256 hex digest of a file's bytes.
fn hash_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).map_err(|source| SecondBrainError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(hex::encode(Sha256::digest(&bytes)))
}
