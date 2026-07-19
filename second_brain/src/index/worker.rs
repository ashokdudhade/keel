//! Parallel file parsing pipeline. CPU-bound parse/extract runs in parallel;
//! database writes are serialized by the orchestrator in `index::index_repository`.

use crate::error::{Result, SecondBrainError};
use crate::graph::types::{FileNode, ImplRecord, Import, Reference, Symbol};
use crate::languages::Registry;
use ignore::WalkBuilder;
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};

/// A parsed file with its extracted symbols and references.
pub struct ParsedFile {
    /// The indexed file and its content hash.
    pub node: FileNode,
    /// Symbols defined in the file.
    pub symbols: Vec<Symbol>,
    /// References found in the file.
    pub references: Vec<Reference>,
    /// `use`/import records found in the file.
    pub imports: Vec<Import>,
    /// `impl` block records found in the file.
    pub impls: Vec<ImplRecord>,
}

/// Store paths relative to the index `root` (forward-slash normalized keys come
/// from [`PathBuf`]'s platform display; callers use the returned path as the DB key).
pub fn normalize_path(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf())
}

/// Collect all source files under `root` whose extension has a registered
/// plugin, honoring `.gitignore` (via `ignore`).
///
/// Returned paths are absolute WalkBuilder paths; callers should
/// [`normalize_path`] before persisting.
pub fn collect_source_files(root: &Path, registry: &Registry) -> Vec<PathBuf> {
    let exts = registry.extensions();
    let mut files: Vec<PathBuf> = WalkBuilder::new(root)
        .standard_filters(true)
        // Honor `.gitignore` even when `root` is not inside a git repository;
        // by default the `ignore` crate only applies gitignore rules when a
        // `.git` dir is present.
        .require_git(false)
        .build()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|entry| entry.into_path())
        .filter(|path| {
            path.extension()
                .and_then(|s| s.to_str())
                .is_some_and(|ext| exts.contains(&ext))
        })
        .collect();
    // Sort for deterministic insertion / `file_id` order across machines.
    files.sort();
    files
}

/// Parse and extract from already-read UTF-8 `source`, storing `rel_path` on the
/// [`FileNode`].
pub fn parse_file_contents(
    rel_path: &Path,
    source: &str,
    content_hash: String,
    registry: &Registry,
) -> Result<ParsedFile> {
    let ext = rel_path.extension().and_then(|s| s.to_str()).unwrap_or("");
    let plugin = registry
        .for_extension(ext)
        .ok_or_else(|| SecondBrainError::UnsupportedExtension(ext.to_string()))?;

    let symbols = plugin.extract_symbols(rel_path, source)?;
    let references = plugin.extract_references(rel_path, source)?;
    let imports = plugin.extract_imports(rel_path, source)?;
    let impls = plugin.extract_impls(rel_path, source)?;

    Ok(ParsedFile {
        node: FileNode {
            path: rel_path.to_path_buf(),
            content_hash,
        },
        symbols,
        references,
        imports,
        impls,
    })
}

/// Read, parse, and extract a single file (absolute `abs_path`, stored as `rel_path`).
pub fn parse_file(abs_path: &Path, rel_path: &Path, registry: &Registry) -> Result<ParsedFile> {
    let bytes = fs::read(abs_path).map_err(|source| SecondBrainError::Io {
        path: abs_path.to_path_buf(),
        source,
    })?;
    let content_hash = hex::encode(Sha256::digest(&bytes));
    let source = std::str::from_utf8(&bytes).map_err(|_| SecondBrainError::Parse)?;
    parse_file_contents(rel_path, source, content_hash, registry)
}

/// Outcome of hashing (and optionally parsing) one candidate file.
pub enum FileOutcome {
    /// Content hash matched the existing index entry.
    Skipped,
    /// File was parsed successfully.
    Parsed(ParsedFile),
    /// Per-file failure; indexing continues.
    Failed {
        /// Absolute path that failed.
        path: PathBuf,
        /// Error message for stderr logging.
        message: String,
    },
}

/// Hash every file; parse those whose hash changed. Reads each file at most once.
///
/// Failures for individual files become [`FileOutcome::Failed`] rather than
/// aborting the whole batch.
pub fn hash_and_parse(
    root: &Path,
    files: &[PathBuf],
    existing: &std::collections::HashMap<String, String>,
    registry: &Registry,
) -> Vec<FileOutcome> {
    files
        .par_iter()
        .map(|abs_path| {
            let rel = normalize_path(root, abs_path);
            let key = rel.to_string_lossy().into_owned();
            match process_one(abs_path, &rel, &key, existing, registry) {
                Ok(outcome) => outcome,
                Err(e) => FileOutcome::Failed {
                    path: abs_path.clone(),
                    message: e.to_string(),
                },
            }
        })
        .collect()
}

fn process_one(
    abs_path: &Path,
    rel: &Path,
    key: &str,
    existing: &std::collections::HashMap<String, String>,
    registry: &Registry,
) -> Result<FileOutcome> {
    let bytes = fs::read(abs_path).map_err(|source| SecondBrainError::Io {
        path: abs_path.to_path_buf(),
        source,
    })?;
    let hash = hex::encode(Sha256::digest(&bytes));

    if existing.get(key).is_some_and(|prev| prev == &hash) {
        return Ok(FileOutcome::Skipped);
    }

    let source = match std::str::from_utf8(&bytes) {
        Ok(s) => s,
        Err(_) => {
            return Ok(FileOutcome::Failed {
                path: abs_path.to_path_buf(),
                message: "invalid UTF-8".to_string(),
            });
        }
    };

    match parse_file_contents(rel, source, hash, registry) {
        Ok(parsed) => Ok(FileOutcome::Parsed(parsed)),
        Err(e) => Ok(FileOutcome::Failed {
            path: abs_path.to_path_buf(),
            message: e.to_string(),
        }),
    }
}
