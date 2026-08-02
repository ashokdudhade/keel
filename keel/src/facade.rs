//! Stable public library facade for Keel consumers.
//!
//! Prefer [`Index`] over reaching into `db` / `graph` / `index` modules directly.
//! Internals remain available for the CLI and advanced use.

use crate::db::{queries, schema};
use crate::error::Result;
use crate::graph::deps::{self, Dependency};
use crate::graph::impact;
use crate::graph::query_result::QueryResult;
use crate::graph::resolve;
use crate::graph::target;
use crate::graph::types::{ImplRecord, Reference, Symbol};
use crate::index::{self, IndexStats};
use crate::languages::Registry;
use rusqlite::Connection;
use std::path::Path;

/// Opened Keel index (SQLite-backed).
///
/// This is the stable library entry point for indexing and querying.
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
        Ok(self.definition_with_meta(name)?.results)
    }

    /// Definitions plus confidence metadata.
    pub fn definition_with_meta(&self, name: &str) -> Result<QueryResult<Symbol>> {
        definition_with_meta(&self.conn, name)
    }

    /// Find references matching `name`.
    pub fn references(&self, name: &str) -> Result<Vec<Reference>> {
        Ok(self.references_with_meta(name)?.results)
    }

    /// References plus confidence metadata.
    pub fn references_with_meta(&self, name: &str) -> Result<QueryResult<Reference>> {
        references_with_meta(&self.conn, name)
    }

    /// Find callers of `name` (import-aware when a unique definition module exists).
    pub fn callers(&self, name: &str) -> Result<Vec<Reference>> {
        Ok(self.callers_with_meta(name)?.results)
    }

    /// Callers plus confidence metadata.
    pub fn callers_with_meta(&self, name: &str) -> Result<QueryResult<Reference>> {
        callers_with_meta(&self.conn, name)
    }

    /// Find trait implementations for `trait_name`.
    pub fn implementations(&self, trait_name: &str) -> Result<Vec<ImplRecord>> {
        Ok(self.implementations_with_meta(trait_name)?.results)
    }

    /// Implementations plus confidence metadata.
    pub fn implementations_with_meta(
        &self,
        trait_name: &str,
    ) -> Result<QueryResult<ImplRecord>> {
        implementations_with_meta(&self.conn, trait_name)
    }

    /// Find modules/files that `name` (module path or symbol) depends on.
    pub fn dependencies(&self, name: &str) -> Result<Vec<Dependency>> {
        Ok(self.dependencies_with_meta(name)?.results)
    }

    /// Dependencies plus confidence metadata.
    pub fn dependencies_with_meta(&self, name: &str) -> Result<QueryResult<Dependency>> {
        dependencies_with_meta(&self.conn, name)
    }

    /// Find symbols transitively impacted by changing `name`.
    pub fn impact(&self, name: &str) -> Result<Vec<Symbol>> {
        Ok(self.impact_with_meta(name)?.results)
    }

    /// Impact plus confidence metadata.
    pub fn impact_with_meta(&self, name: &str) -> Result<QueryResult<Symbol>> {
        impact_with_meta(&self.conn, name)
    }
}

/// Definitions plus confidence metadata (shared by [`Index`] and MCP/CLI).
pub fn definition_with_meta(conn: &Connection, name: &str) -> Result<QueryResult<Symbol>> {
    let results = queries::find_definition(conn, name)?;
    let multi = results.len() > 1;
    let tiers: Vec<u8> = if results.is_empty() {
        vec![]
    } else if multi {
        vec![3; results.len()]
    } else {
        vec![2]
    };
    let mut notes = Vec::new();
    if multi {
        notes.push(format!(
            "Found {} definitions for `{name}`; disambiguate by module if needed.",
            results.len()
        ));
    }
    Ok(QueryResult::from_tiers(results, &tiers, multi, notes))
}

/// References plus confidence metadata.
pub fn references_with_meta(conn: &Connection, name: &str) -> Result<QueryResult<Reference>> {
    let results = queries::find_references(conn, name)?;
    let defs = queries::find_definition(conn, name)?;
    let multi = defs.len() > 1;
    let tiers: Vec<u8> = if results.is_empty() {
        vec![]
    } else if multi {
        vec![3; results.len()]
    } else {
        vec![2; results.len()]
    };
    let mut notes = Vec::new();
    if multi {
        notes.push("Multiple definitions share this name; references are name-matched.".into());
    }
    Ok(QueryResult::from_tiers(results, &tiers, multi, notes))
}

/// Callers plus confidence metadata.
pub fn callers_with_meta(conn: &Connection, name: &str) -> Result<QueryResult<Reference>> {
    let defs = queries::find_definition(conn, name)?;
    let multi = defs.len() > 1;
    let target_module = unique_module(&defs);
    let results = resolve::find_callers(conn, name, target_module.as_deref())?;
    let (tiers, notes) = if target_module.is_some() {
        (vec![1; results.len().max(1)], Vec::new())
    } else {
        let mut n = Vec::new();
        if multi {
            n.push("No unique definition module; callers fall back to name matching.".into());
        }
        (vec![3; results.len().max(1)], n)
    };
    let tiers = if results.is_empty() { vec![] } else { tiers };
    Ok(QueryResult::from_tiers(results, &tiers, multi, notes))
}

/// Implementations plus confidence metadata.
pub fn implementations_with_meta(
    conn: &Connection,
    trait_name: &str,
) -> Result<QueryResult<ImplRecord>> {
    let results = queries::find_implementations(conn, trait_name)?;
    let mut notes = Vec::new();
    if results.is_empty() {
        notes.push(
            "No implementations found (Rust traits only today; other languages stay empty when unambiguous extraction is unavailable)."
                .into(),
        );
    }
    let tiers = if results.is_empty() {
        vec![]
    } else {
        vec![2; results.len()]
    };
    Ok(QueryResult::from_tiers(results, &tiers, false, notes))
}

/// Dependencies plus confidence metadata.
pub fn dependencies_with_meta(
    conn: &Connection,
    name: &str,
) -> Result<QueryResult<Dependency>> {
    let resolved = target::normalize_target(conn, name)?;
    let results = deps::find_dependencies(conn, name)?;
    let mut notes = Vec::new();
    if resolved.files.is_empty() {
        notes.push(format!("No indexed files found for target `{name}`."));
        return Ok(QueryResult::from_tiers(results, &[], false, notes));
    }
    if results.is_empty() {
        notes.push(format!(
            "Target resolved to {} file(s) but no import/cross-file dependencies were recorded.",
            resolved.files.len()
        ));
        return Ok(QueryResult::from_tiers(results, &[], false, notes));
    }
    Ok(QueryResult::from_tiers(results, &[1], false, notes))
}

/// Impact plus confidence metadata.
pub fn impact_with_meta(conn: &Connection, name: &str) -> Result<QueryResult<Symbol>> {
    let defs = queries::find_definition(conn, name)?;
    let multi = defs.len() > 1;
    let results = impact::find_impact(conn, name)?;
    let mut notes = Vec::new();
    if multi {
        notes.push(
            "Multiple definitions; impact expands per qualified identity and may over-approximate."
                .into(),
        );
    }
    let tiers = if results.is_empty() {
        vec![]
    } else if multi {
        vec![2, 3]
    } else {
        vec![2; results.len().min(8)]
    };
    Ok(QueryResult::from_tiers(results, &tiers, multi, notes))
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
    use crate::graph::query_result::Confidence;
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

        let meta = index.definition_with_meta("AuthService").unwrap();
        assert_eq!(meta.confidence, Confidence::High);
    }

    #[test]
    fn dependencies_resolve_rust_file_module() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/mcp")).unwrap();
        fs::create_dir_all(root.join("src/api")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "mod api;\nmod mcp;\n",
        )
        .unwrap();
        fs::write(
            root.join("src/api/mod.rs"),
            "pub struct Token;\n",
        )
        .unwrap();
        fs::write(
            root.join("src/mcp/mod.rs"),
            "use crate::api::Token;\npub fn serve() {}\n",
        )
        .unwrap();

        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        let serve = index.definition("serve").unwrap();
        assert_eq!(serve.len(), 1);
        assert_eq!(serve[0].module_path, "crate::mcp");

        let deps = index.dependencies_with_meta("crate::mcp").unwrap();
        assert!(
            !deps.results.is_empty(),
            "expected imports for crate::mcp, got notes={:?}",
            deps.notes
        );
        let paths: Vec<_> = deps.results.iter().map(|d| d.module_path.as_str()).collect();
        assert!(
            paths.iter().any(|p| *p == "crate::api" || p.starts_with("crate::api")),
            "unexpected deps={paths:?}"
        );
        assert_eq!(deps.confidence, Confidence::High);
    }
}
