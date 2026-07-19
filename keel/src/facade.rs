//! Stable public library facade for Keel 1.0 consumers.
//!
//! Prefer [`Index`] over reaching into `db` / `graph` / `index` modules directly.
//! Internals remain available for the CLI and advanced use.

use crate::db::{queries, schema};
use crate::error::Result;
use crate::graph::deps::{self, Dependency};
use crate::graph::impact;
use crate::graph::resolve;
use crate::graph::types::{ImplRecord, Reference, Symbol};
use crate::index::{self, IndexStats};
use crate::languages::Registry;
use rusqlite::Connection;
use std::path::Path;

/// Opened Keel index (SQLite-backed).
///
/// This is the stable 1.0 library entry point for indexing and querying.
pub struct Index {
    conn: Connection,
}

impl Index {
    /// Open (or create) an on-disk index database at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path.as_ref())?;
        crate::db::configure_connection(&conn)?;
        schema::initialize(&conn)?;
        Ok(Self { conn })
    }

    /// Open an in-memory index (useful for tests and ephemeral analysis).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        crate::db::configure_connection(&conn)?;
        schema::initialize(&conn)?;
        Ok(Self { conn })
    }

    /// Index every registered-language source file under `root`.
    pub fn index_path(&mut self, root: &Path) -> Result<IndexStats> {
        index::index_repository(root, &mut self.conn)
    }

    /// Index `root` using a custom [`Registry`] (community language plugins).
    pub fn index_path_with(
        &mut self,
        root: &Path,
        registry: &Registry,
    ) -> Result<IndexStats> {
        index::index_repository_with(root, &mut self.conn, registry)
    }

    /// Find definitions matching `name`.
    pub fn definition(&self, name: &str) -> Result<Vec<Symbol>> {
        queries::find_definition(&self.conn, name)
    }

    /// Find references matching `name`.
    pub fn references(&self, name: &str) -> Result<Vec<Reference>> {
        queries::find_references(&self.conn, name)
    }

    /// Find callers of `name` (import-aware when a unique definition module exists).
    pub fn callers(&self, name: &str) -> Result<Vec<Reference>> {
        let defs = queries::find_definition(&self.conn, name)?;
        let target_module = unique_module(&defs);
        resolve::find_callers(&self.conn, name, target_module.as_deref())
    }

    /// Find trait implementations for `trait_name`.
    pub fn implementations(&self, trait_name: &str) -> Result<Vec<ImplRecord>> {
        queries::find_implementations(&self.conn, trait_name)
    }

    /// Find modules/files that `name` (module path or symbol) depends on.
    pub fn dependencies(&self, name: &str) -> Result<Vec<Dependency>> {
        deps::find_dependencies(&self.conn, name)
    }

    /// Find symbols transitively impacted by changing `name`.
    pub fn impact(&self, name: &str) -> Result<Vec<Symbol>> {
        impact::find_impact(&self.conn, name)
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::types::SymbolKind;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn index_facade_indexes_and_finds_definition() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "pub struct AuthService;\nfn create_order() {}\n",
        )
        .unwrap();

        let mut index = Index::open_in_memory().unwrap();
        let stats = index.index_path(root).unwrap();
        assert_eq!(stats.indexed, 1);

        let defs = index.definition("AuthService").unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].kind, SymbolKind::Struct);
        assert_eq!(defs[0].name, "AuthService");
    }
}
