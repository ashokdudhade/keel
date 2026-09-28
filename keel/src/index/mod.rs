//! Indexing orchestration: crawl, parse in parallel, then persist in one
//! transaction.

pub mod watch;
pub mod worker;

use crate::db::{queries, schema};
use crate::error::Result;
use crate::languages::Registry;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Counts of files processed by an incremental indexing pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IndexStats {
    /// Files that were parsed and written (new or content-changed).
    pub indexed: usize,
    /// Files whose content hash matched the existing index and were left alone.
    pub skipped: usize,
    /// Files present in the DB but no longer on disk, whose rows were deleted.
    pub removed: usize,
    /// Files that failed to hash or parse (indexing continued for others).
    pub errors: usize,
}

/// Index every registered-language source file under `root` into `conn`.
///
/// Uses [`Registry::with_defaults`]. Prefer [`index_repository_with`] when
/// injecting community plugins.
///
/// Incremental: hashes candidate files first, skips unchanged paths, parses and
/// persists new/changed files, and deletes DB rows for files gone from disk.
/// Paths are stored relative to `root`. Per-file failures are counted in
/// [`IndexStats::errors`] and do not abort the pass.
pub fn index_repository(root: &Path, conn: &mut Connection) -> Result<IndexStats> {
    let registry = Registry::with_defaults();
    index_repository_with(root, conn, &registry)
}

/// Index every source file under `root` whose extension is claimed by
/// `registry`.
///
/// Incremental: hashes candidate files first, skips unchanged paths, parses and
/// persists new/changed files, and deletes DB rows for files gone from disk.
pub fn index_repository_with(
    root: &Path,
    conn: &mut Connection,
    registry: &Registry,
) -> Result<IndexStats> {
    schema::initialize(conn)?;
    // Stale content (older writer): wipe and re-parse everything rather than
    // serve answers with outdated semantics. Upgrades become seamless: the
    // first index pass after an update rebuilds automatically.
    let stale = schema::index_format_version(conn) != schema::INDEX_FORMAT_VERSION;
    if stale {
        eprintln!("keel: index format changed; rebuilding index from scratch.");
    }
    let abs_files = worker::collect_source_files(root, registry);
    // Old hashes are meaningless after a format change: re-parse every file.
    let existing: HashMap<String, String> = if stale {
        HashMap::new()
    } else {
        queries::existing_hashes(conn)?
    };

    let outcomes = worker::hash_and_parse(root, &abs_files, &existing, registry);

    let mut parsed = Vec::new();
    let mut skipped = 0usize;
    let mut errors = 0usize;
    let mut on_disk: HashSet<String> = HashSet::with_capacity(abs_files.len());

    for abs in &abs_files {
        let rel = worker::normalize_path(root, abs);
        on_disk.insert(rel.to_string_lossy().into_owned());
    }

    for outcome in outcomes {
        match outcome {
            worker::FileOutcome::Skipped => skipped += 1,
            worker::FileOutcome::Parsed(pf) => parsed.push(pf),
            worker::FileOutcome::Failed { path, message } => {
                errors += 1;
                eprintln!("index error: {}: {message}", path.display());
            }
        }
    }

    let mut removed = 0usize;
    let tx = conn.transaction()?;
    if stale {
        queries::clear_index(&tx)?;
    }
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
    if stale {
        // Stamped inside the same transaction: a failed pass stays stale and
        // retries the rebuild next time instead of marking partial content.
        schema::stamp_index_format(&tx, schema::INDEX_FORMAT_VERSION)?;
    }
    schema::stamp_last_indexed(&tx)?;
    tx.commit()?;

    Ok(IndexStats {
        indexed: parsed.len(),
        skipped,
        removed,
        errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn stores_paths_relative_to_root() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "fn hello() {}\n").unwrap();

        let mut conn = Connection::open_in_memory().unwrap();
        let stats = index_repository(root, &mut conn).unwrap();
        assert_eq!(stats.indexed, 1);
        assert_eq!(stats.errors, 0);

        let path: String = conn
            .query_row("SELECT path FROM files", [], |row| row.get(0))
            .unwrap();
        assert_eq!(path, "src/lib.rs");
        assert!(!path.starts_with('/'));
    }

    #[test]
    fn stale_format_wipes_and_rebuilds_content() {
        use crate::db::{queries, schema};
        use crate::graph::types::{Symbol, SymbolKind};
        use std::path::PathBuf;

        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "fn hello() {}\n").unwrap();

        let mut conn = Connection::open_in_memory().unwrap();
        index_repository(root, &mut conn).unwrap();

        // Simulate an older writer: stale stamp plus a bogus row the new
        // extractor would never produce.
        schema::stamp_index_format(&conn, 0).unwrap();
        let file_id = queries::insert_file(
            &conn,
            &crate::graph::types::FileNode {
                path: PathBuf::from("src/ghost.rs"),
                content_hash: "bogus".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            &conn,
            file_id,
            &[Symbol {
                name: "Ghost".into(),
                kind: SymbolKind::Struct,
                file: PathBuf::new(),
                start_line: 1,
                start_col: 1,
                module_path: String::new(),
            }],
        )
        .unwrap();

        let stats = index_repository(root, &mut conn).unwrap();
        assert_eq!(stats.indexed, 1);
        assert_eq!(stats.skipped, 0);
        assert_eq!(
            schema::index_format_version(&conn),
            schema::INDEX_FORMAT_VERSION
        );
        assert!(queries::find_definition(&conn, "Ghost").unwrap().is_empty());
        assert_eq!(queries::find_definition(&conn, "hello").unwrap().len(), 1);
    }

    #[test]
    fn continues_when_one_file_fails_utf8() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/ok.rs"), "fn ok() {}\n").unwrap();
        fs::write(root.join("src/bad.rs"), [0xff, 0xfe, b'f', b'n']).unwrap();

        let mut conn = Connection::open_in_memory().unwrap();
        let stats = index_repository(root, &mut conn).unwrap();
        assert_eq!(stats.indexed, 1);
        assert_eq!(stats.errors, 1);

        let defs = crate::db::queries::find_definition(&conn, "ok").unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].file.as_os_str(), "src/ok.rs");
    }
}
