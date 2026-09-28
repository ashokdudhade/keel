//! Indexing orchestration: crawl, parse in parallel, then persist in one
//! transaction.

pub mod watch;
pub mod worker;

use crate::db::{queries, schema};
use crate::error::Result;
use crate::graph::types::FileNode;
use crate::languages::Registry;
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Per-file stderr diagnostics are capped at this many lines per pass;
/// beyond it a single summary line carries the remainder (large repos
/// can otherwise print hundreds of warnings).
const MAX_FILE_DIAGNOSTICS: usize = 10;

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
    /// Files indexed despite syntax errors (extraction is partial; the
    /// first few name themselves on stderr, the rest fold into a summary).
    pub syntax_errors: usize,
}

/// Index every registered-language source file under `root` into `conn`.
///
/// Uses [`Registry::with_defaults`]. Prefer [`index_repository_with`] when
/// injecting community plugins.
///
/// Incremental: hashes candidate files first, skips unchanged paths, parses and
/// persists new/changed files, and deletes DB rows for files gone from disk.
/// Paths are stored relative to `root`. Per-file failures are counted in
/// [`IndexStats::errors`] and do not abort the pass; hashed failures are
/// remembered so unchanged bad files skip (rather than re-error) next pass.
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

    let crates = worker::rust_crate_names(root, &abs_files);
    let outcomes = worker::hash_and_parse(root, &abs_files, &existing, registry, &crates);

    let mut parsed = Vec::new();
    let mut failed_hashes: Vec<FileNode> = Vec::new();
    let mut skipped = 0usize;
    let mut errors = 0usize;
    let mut syntax_errors = 0usize;
    let mut on_disk: HashSet<String> = HashSet::with_capacity(abs_files.len());

    for abs in &abs_files {
        let rel = worker::normalize_path(root, abs);
        on_disk.insert(rel.to_string_lossy().into_owned());
    }

    for outcome in outcomes {
        match outcome {
            worker::FileOutcome::Skipped => skipped += 1,
            worker::FileOutcome::Parsed(pf) => {
                if pf.syntax_error {
                    syntax_errors += 1;
                    if syntax_errors <= MAX_FILE_DIAGNOSTICS {
                        eprintln!(
                            "index warning: {}: syntax errors; symbols may be incomplete",
                            pf.node.path.display()
                        );
                    }
                }
                parsed.push(pf);
            }
            worker::FileOutcome::Failed {
                path,
                message,
                hash,
            } => {
                errors += 1;
                if errors <= MAX_FILE_DIAGNOSTICS {
                    eprintln!("index error: {}: {message}", path.display());
                }
                // Remember hashed failures so unchanged bad files skip
                // (instead of re-erroring) on later passes. A content
                // change re-attempts the file, so fixing it re-indexes.
                if let Some(content_hash) = hash {
                    failed_hashes.push(FileNode {
                        path: worker::normalize_path(root, &path).to_path_buf(),
                        content_hash,
                    });
                }
            }
        }
    }
    if syntax_errors > MAX_FILE_DIAGNOSTICS {
        eprintln!(
            "index warning: ... and {} more file(s) with syntax errors",
            syntax_errors - MAX_FILE_DIAGNOSTICS
        );
    }
    if errors > MAX_FILE_DIAGNOSTICS {
        eprintln!(
            "index error: ... and {} more file(s) failed to index",
            errors - MAX_FILE_DIAGNOSTICS
        );
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
    for node in &failed_hashes {
        // Hash-only row: no symbols to store, but the hash makes the next
        // pass skip the unchanged file instead of re-reporting the error.
        // Clearing removes stale rows when a previously-good file breaks.
        let file_id = queries::insert_file(&tx, node)?;
        queries::clear_file_rows(&tx, file_id)?;
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
        syntax_errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn syntax_error_files_are_counted_not_failed() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("ok.py"), "def f():\n    pass\n").unwrap();
        // One broken file per grammar (including the tsx/jsx-only plugins):
        // each must set the syntax flag while still indexing.
        for (name, src) in [
            ("bad.rs", "fn broken( {\n"),
            ("bad.py", "def broken(:\n  ???\n"),
            ("bad.go", "package p\nfunc broken( {\n"),
            ("bad.js", "function broken( {\n"),
            ("bad.ts", "function broken( : string {\n"),
            ("bad.tsx", "const x = <div;\n"),
            ("bad.jsx", "const x = <div;\n"),
        ] {
            fs::write(root.join(name), src).unwrap();
        }

        let mut conn = Connection::open_in_memory().unwrap();
        let stats = index_repository(root, &mut conn).unwrap();
        assert_eq!(stats.indexed, 8, "broken files still index: {stats:?}");
        assert_eq!(stats.errors, 0, "{stats:?}");
        assert_eq!(stats.syntax_errors, 7, "{stats:?}");

        // A clean re-index touches nothing and re-reports nothing.
        let again = index_repository(root, &mut conn).unwrap();
        assert_eq!(again.skipped, 8);
        assert_eq!(again.syntax_errors, 0);
    }

    #[test]
    fn hard_error_files_are_remembered_until_changed() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("ok.py"), "def f():\n    pass\n").unwrap();
        fs::write(root.join("bad.py"), b"def f():\n    return \xff\xfe binary\n").unwrap();

        let mut conn = Connection::open_in_memory().unwrap();
        let first = index_repository(root, &mut conn).unwrap();
        assert_eq!(first.indexed, 1, "{first:?}");
        assert_eq!(first.errors, 1, "{first:?}");

        // Unchanged: the bad file is skipped, not re-reported, on every
        // later pass (otherwise each query re-prints the same error).
        let again = index_repository(root, &mut conn).unwrap();
        assert_eq!(again.errors, 0, "{again:?}");
        assert_eq!(again.skipped, 2, "{again:?}");

        // Fixing the file re-indexes it with real symbols.
        fs::write(root.join("bad.py"), "def fixed():\n    pass\n").unwrap();
        let fixed = index_repository(root, &mut conn).unwrap();
        assert_eq!(fixed.errors, 0, "{fixed:?}");
        assert_eq!(fixed.indexed, 1, "{fixed:?}");
        let defs = crate::db::queries::find_definition(&conn, "fixed").unwrap();
        assert_eq!(defs.len(), 1, "fixed file contributes symbols");
    }

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

    #[test]
    fn prunes_vendor_and_build_dirs_without_gitignore() {
        use crate::index::worker::collect_source_files;
        use crate::languages::Registry;

        let dir = tempdir().unwrap();
        let root = dir.path();
        // No `.gitignore` on purpose: junk dirs must be pruned regardless.
        for sub in ["src", "node_modules/dep", "target/debug", "dist", "vendor/x"] {
            fs::create_dir_all(root.join(sub)).unwrap();
            fs::write(root.join(sub).join("lib.js"), "export function lib() {}\n").unwrap();
        }
        // Hand-written lookalikes must survive.
        fs::create_dir_all(root.join("build")).unwrap();
        fs::write(root.join("build/webpack.config.js"), "export default {};\n").unwrap();

        let registry = Registry::with_defaults();
        let files = collect_source_files(root, &registry);
        let rel: Vec<String> = files
            .iter()
            .map(|p| {
                p.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(rel, vec!["build/webpack.config.js", "src/lib.js"]);
    }

    #[test]
    fn keelignore_excludes_alongside_gitignore() {
        use crate::index::worker::collect_source_files;
        use crate::languages::Registry;

        let dir = tempdir().unwrap();
        let root = dir.path();
        for sub in ["src", "generated", "vendor-git"] {
            fs::create_dir_all(root.join(sub)).unwrap();
            fs::write(root.join(sub).join("lib.js"), "export function lib() {}\n").unwrap();
        }
        fs::write(root.join("src/local.g.js"), "export function g() {}\n").unwrap();
        fs::write(root.join(".gitignore"), "vendor-git/\n").unwrap();
        fs::write(root.join(".keelignore"), "generated/\n*.g.js\n").unwrap();

        let registry = Registry::with_defaults();
        let files = collect_source_files(root, &registry);
        let rel: Vec<String> = files
            .iter()
            .map(|p| {
                p.strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(rel, vec!["src/lib.js"]);
    }

    #[test]
    fn walk_root_named_like_junk_dir_still_indexes() {
        use crate::index::worker::collect_source_files;
        use crate::languages::Registry;

        let dir = tempdir().unwrap();
        // Project rooted in `target/`: depth-0 must never be pruned.
        let root = dir.path().join("target");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "fn hello() {}\n").unwrap();

        let registry = Registry::with_defaults();
        let files = collect_source_files(&root, &registry);
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("src/lib.rs"));
    }

    #[test]
    fn same_crate_use_paths_normalize_to_crate_roots() {
        use crate::db::queries;
        use crate::graph::resolve::find_alias_uses;

        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"m\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        fs::write(root.join("src/lib.rs"), "pub mod store {\n    pub struct Mem;\n}\n").unwrap();
        fs::write(
            root.join("src/main.rs"),
            "use m::store::Mem as Store;\n\npub fn run(s: Store) -> u32 {\n    1\n}\n",
        )
        .unwrap();

        let mut conn = Connection::open_in_memory().unwrap();
        let stats = index_repository(root, &mut conn).unwrap();
        assert_eq!(stats.errors, 0);

        // The crate-named import resolves to the `crate::` module, so the
        // aliased use is found through the target symbol.
        let paths: Vec<String> = queries::aliased_imports(&conn)
            .unwrap()
            .iter()
            .map(|(_, module_path, _)| module_path.clone())
            .collect();
        assert_eq!(paths, vec!["crate::store::Mem".to_string()]);
        let uses = find_alias_uses(&conn, "Mem", None).unwrap();
        assert_eq!(uses.len(), 1);
        assert_eq!(uses[0].name, "Store");
    }
}
