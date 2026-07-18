//! Parallel file parsing pipeline. CPU-bound parse/extract runs in parallel;
//! database writes are serialized by the orchestrator in `index::index_repository`.

use crate::error::{Result, SecondBrainError};
use crate::graph::types::{FileNode, Reference, Symbol};
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
}

/// Collect all `.rs` files under `root`, honoring `.gitignore` (via `ignore`).
pub fn collect_rust_files(root: &Path) -> Vec<PathBuf> {
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
        .filter(|path| path.extension().and_then(|s| s.to_str()) == Some("rs"))
        .collect();
    // Sort for deterministic insertion / `file_id` order across machines.
    files.sort();
    files
}

/// Read, parse, and extract a single file.
pub fn parse_file(path: &Path, registry: &Registry) -> Result<ParsedFile> {
    let source = fs::read_to_string(path)
        .map_err(|source| SecondBrainError::Io { path: path.to_path_buf(), source })?;
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    let plugin = registry
        .for_extension(ext)
        .ok_or_else(|| SecondBrainError::UnsupportedExtension(ext.to_string()))?;

    let symbols = plugin.extract_symbols(&source)?;
    let references = plugin.extract_references(&source)?;
    let content_hash = hex::encode(Sha256::digest(source.as_bytes()));

    Ok(ParsedFile {
        node: FileNode { path: path.to_path_buf(), content_hash },
        symbols,
        references,
    })
}

/// Parse every file in parallel. Fails fast on the first error.
pub fn parse_all(files: &[PathBuf], registry: &Registry) -> Result<Vec<ParsedFile>> {
    files.par_iter().map(|path| parse_file(path, registry)).collect()
}
