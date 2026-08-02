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

    /// Definitions with optional module disambiguation.
    pub fn definition_with_meta_opts(
        &self,
        name: &str,
        module: Option<&str>,
    ) -> Result<QueryResult<Symbol>> {
        definition_with_meta_opts(&self.conn, name, module)
    }

    /// Find references matching `name`.
    pub fn references(&self, name: &str) -> Result<Vec<Reference>> {
        Ok(self.references_with_meta(name)?.results)
    }

    /// References plus confidence metadata.
    pub fn references_with_meta(&self, name: &str) -> Result<QueryResult<Reference>> {
        references_with_meta(&self.conn, name)
    }

    /// References with optional module disambiguation for the defining symbol.
    pub fn references_with_meta_opts(
        &self,
        name: &str,
        module: Option<&str>,
    ) -> Result<QueryResult<Reference>> {
        references_with_meta_opts(&self.conn, name, module)
    }

    /// Find callers of `name` (import-aware when a unique definition module exists).
    pub fn callers(&self, name: &str) -> Result<Vec<Reference>> {
        Ok(self.callers_with_meta(name)?.results)
    }

    /// Callers plus confidence metadata.
    pub fn callers_with_meta(&self, name: &str) -> Result<QueryResult<Reference>> {
        callers_with_meta(&self.conn, name)
    }

    /// Callers with optional module disambiguation.
    pub fn callers_with_meta_opts(
        &self,
        name: &str,
        module: Option<&str>,
    ) -> Result<QueryResult<Reference>> {
        callers_with_meta_opts(&self.conn, name, module)
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

    /// Impact with optional module disambiguation.
    pub fn impact_with_meta_opts(
        &self,
        name: &str,
        module: Option<&str>,
    ) -> Result<QueryResult<Symbol>> {
        impact_with_meta_opts(&self.conn, name, module)
    }
}

/// Split `module::…::symbol` into `(module_path, symbol)` when unambiguous.
///
/// Returns `None` when there is no `::` separator (bare symbol).
pub fn split_qualified_name(name: &str) -> Option<(&str, &str)> {
    let (module, symbol) = name.rsplit_once("::")?;
    if module.is_empty() || symbol.is_empty() || symbol.contains("::") {
        return None;
    }
    Some((module, symbol))
}

/// Resolve optional `module` arg or a qualified `name` into `(module, bare_name)`.
///
/// When both `module` and a qualified `name` are provided, the last `::` segment
/// is the symbol and `module` wins as the module path (agents often pass both).
fn resolve_symbol_target<'a>(
    name: &'a str,
    module: Option<&'a str>,
) -> (Option<&'a str>, &'a str) {
    if let Some(m) = module {
        let bare = split_qualified_name(name).map(|(_, sym)| sym).unwrap_or(name);
        return (Some(m), bare);
    }
    if let Some((m, sym)) = split_qualified_name(name) {
        return (Some(m), sym);
    }
    (None, name)
}

/// Definitions plus confidence metadata (shared by [`Index`] and MCP/CLI).
pub fn definition_with_meta(conn: &Connection, name: &str) -> Result<QueryResult<Symbol>> {
    definition_with_meta_opts(conn, name, None)
}

/// Definitions with optional module filter (or qualified `name`).
pub fn definition_with_meta_opts(
    conn: &Connection,
    name: &str,
    module: Option<&str>,
) -> Result<QueryResult<Symbol>> {
    let (mod_path, bare) = resolve_symbol_target(name, module);
    let results = if let Some(m) = mod_path {
        queries::find_definition_by_qualified(conn, m, bare)?
    } else {
        queries::find_definition(conn, bare)?
    };
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
            "Found {} definitions for `{bare}`; disambiguate with module arg or qualified name (e.g. `crate::mcp::{bare}`).",
            results.len()
        ));
    } else if results.is_empty() {
        if let Some(m) = mod_path {
            notes.push(format!("No definition for `{bare}` in module `{m}`."));
        }
    }
    Ok(QueryResult::from_tiers(results, &tiers, multi, notes))
}

/// References plus confidence metadata.
pub fn references_with_meta(conn: &Connection, name: &str) -> Result<QueryResult<Reference>> {
    references_with_meta_opts(conn, name, None)
}

/// References with optional module disambiguation for the defining symbol.
pub fn references_with_meta_opts(
    conn: &Connection,
    name: &str,
    module: Option<&str>,
) -> Result<QueryResult<Reference>> {
    let (mod_path, bare) = resolve_symbol_target(name, module);
    let defs = if let Some(m) = mod_path {
        queries::find_definition_by_qualified(conn, m, bare)?
    } else {
        queries::find_definition(conn, bare)?
    };
    let multi = defs.len() > 1;
    let target_module = mod_path
        .map(str::to_owned)
        .or_else(|| unique_module(&defs));
    // When the definition identity is known, filter refs the same way callers do
    // (module/import-aware). Never report High on unfiltered name matches after
    // a module was requested.
    let results = resolve::find_callers(conn, bare, target_module.as_deref())?;
    let (tiers, mut notes) = if target_module.is_some() && !multi {
        (vec![1; results.len().max(1)], Vec::new())
    } else if multi {
        (
            vec![3; results.len().max(1)],
            vec![
                "Multiple definitions share this name; references fall back to name matching. Pass module or a qualified name to narrow."
                    .into(),
            ],
        )
    } else {
        (vec![2; results.len().max(1)], Vec::new())
    };
    let tiers = if results.is_empty() { vec![] } else { tiers };
    if results.is_empty() && defs.is_empty() {
        if let Some(m) = mod_path {
            notes.push(format!("No definition for `{bare}` in module `{m}`."));
        }
    }
    Ok(QueryResult::from_tiers(results, &tiers, multi, notes))
}

/// Callers plus confidence metadata.
pub fn callers_with_meta(conn: &Connection, name: &str) -> Result<QueryResult<Reference>> {
    callers_with_meta_opts(conn, name, None)
}

/// Callers with optional module disambiguation.
pub fn callers_with_meta_opts(
    conn: &Connection,
    name: &str,
    module: Option<&str>,
) -> Result<QueryResult<Reference>> {
    let (mod_path, bare) = resolve_symbol_target(name, module);
    let defs = if let Some(m) = mod_path {
        queries::find_definition_by_qualified(conn, m, bare)?
    } else {
        queries::find_definition(conn, bare)?
    };
    let multi = defs.len() > 1;
    let target_module = mod_path
        .map(str::to_owned)
        .or_else(|| unique_module(&defs));
    let results = resolve::find_callers(conn, bare, target_module.as_deref())?;
    let (tiers, mut notes) = if target_module.is_some() && !multi {
        (vec![1; results.len().max(1)], Vec::new())
    } else {
        let mut n = Vec::new();
        if multi {
            n.push(
                "No unique definition module; callers fall back to name matching. Pass module or a qualified name."
                    .into(),
            );
        }
        (vec![3; results.len().max(1)], n)
    };
    let tiers = if results.is_empty() { vec![] } else { tiers };
    if results.is_empty() && defs.is_empty() {
        if let Some(m) = mod_path {
            notes.push(format!("No definition for `{bare}` in module `{m}`."));
        }
    }
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
    impact_with_meta_opts(conn, name, None)
}

/// Impact with optional module disambiguation.
pub fn impact_with_meta_opts(
    conn: &Connection,
    name: &str,
    module: Option<&str>,
) -> Result<QueryResult<Symbol>> {
    use crate::graph::query_result::{Confidence, ResolutionTier};

    let (mod_path, bare) = resolve_symbol_target(name, module);
    let defs = if let Some(m) = mod_path {
        queries::find_definition_by_qualified(conn, m, bare)?
    } else {
        queries::find_definition(conn, bare)?
    };
    let multi = defs.len() > 1;
    let mut notes = Vec::new();

    if defs.is_empty() {
        if let Some(m) = mod_path {
            notes.push(format!("No definition for `{bare}` in module `{m}`."));
        }
        return Ok(QueryResult::from_tiers(Vec::new(), &[], false, notes));
    }

    let results = impact::find_impact_from_defs(conn, &defs)?;

    if multi {
        notes.push(
            "Multiple definitions; impact expands each qualified identity and may over-approximate. Pass module or a qualified name to narrow."
                .into(),
        );
    }
    if results.is_empty() {
        return Ok(QueryResult::from_tiers(results, &[], multi, notes));
    }

    // Impact is always a candidate radius — never High, never fabricated per-hit tiers.
    notes.push(
        "Impact is a candidate blast radius and may over-approximate; verify before edits.".into(),
    );
    let confidence = if multi {
        Confidence::Low
    } else {
        Confidence::Medium
    };
    let tier = if multi {
        ResolutionTier::Mixed
    } else {
        ResolutionTier::Single(2)
    };
    Ok(QueryResult::new(results, confidence, tier, notes))
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

    #[test]
    fn missing_definition_is_honest_not_found() {
        let index = Index::open_in_memory().unwrap();
        let meta = index.definition_with_meta("NonexistentSymbolXYZ").unwrap();
        assert!(meta.results.is_empty());
        assert_eq!(meta.confidence, Confidence::High);
        assert_eq!(meta.notes, vec!["No matching symbols found.".to_string()]);
    }

    #[test]
    fn multi_def_notes_omit_impact_wording() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/api")).unwrap();
        fs::create_dir_all(root.join("src/mcp")).unwrap();
        fs::write(root.join("src/lib.rs"), "mod api;\nmod mcp;\n").unwrap();
        fs::write(root.join("src/api/mod.rs"), "pub fn serve() {}\n").unwrap();
        fs::write(root.join("src/mcp/mod.rs"), "pub fn serve() {}\n").unwrap();

        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        let meta = index.definition_with_meta("serve").unwrap();
        assert_eq!(meta.results.len(), 2);
        assert!(!meta
            .notes
            .iter()
            .any(|n| n.contains("over-approximate impact")));
        assert!(meta.notes.iter().any(|n| n.contains("disambiguate")));
    }

    #[test]
    fn definition_accepts_qualified_module_path() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/api")).unwrap();
        fs::create_dir_all(root.join("src/mcp")).unwrap();
        fs::write(root.join("src/lib.rs"), "mod api;\nmod mcp;\n").unwrap();
        fs::write(root.join("src/api/mod.rs"), "pub fn serve() {}\n").unwrap();
        fs::write(root.join("src/mcp/mod.rs"), "pub fn serve() {}\n").unwrap();

        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        let meta = index.definition_with_meta("crate::mcp::serve").unwrap();
        assert_eq!(meta.results.len(), 1);
        assert_eq!(meta.results[0].module_path, "crate::mcp");
        assert_eq!(meta.confidence, Confidence::High);
    }

    #[test]
    fn definition_accepts_explicit_module_filter() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/api")).unwrap();
        fs::create_dir_all(root.join("src/mcp")).unwrap();
        fs::write(root.join("src/lib.rs"), "mod api;\nmod mcp;\n").unwrap();
        fs::write(root.join("src/api/mod.rs"), "pub fn serve() {}\n").unwrap();
        fs::write(root.join("src/mcp/mod.rs"), "pub fn serve() {}\n").unwrap();

        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        let meta = index
            .definition_with_meta_opts("serve", Some("crate::mcp"))
            .unwrap();
        assert_eq!(meta.results.len(), 1);
        assert_eq!(meta.results[0].module_path, "crate::mcp");
    }

    #[test]
    fn definition_module_plus_qualified_name_uses_bare_symbol() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/mcp")).unwrap();
        fs::write(root.join("src/lib.rs"), "mod mcp;\n").unwrap();
        fs::write(root.join("src/mcp/mod.rs"), "pub fn serve() {}\n").unwrap();

        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        // Agents sometimes pass both module and a qualified name.
        let meta = index
            .definition_with_meta_opts("crate::mcp::serve", Some("crate::mcp"))
            .unwrap();
        assert_eq!(meta.results.len(), 1);
        assert_eq!(meta.results[0].name, "serve");
        assert_eq!(meta.results[0].module_path, "crate::mcp");
    }

    #[test]
    fn references_with_module_filter_to_resolved_target() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/api")).unwrap();
        fs::create_dir_all(root.join("src/mcp")).unwrap();
        fs::write(root.join("src/lib.rs"), "mod api;\nmod mcp;\nfn boot() { mcp::serve(); }\n").unwrap();
        fs::write(root.join("src/api/mod.rs"), "pub fn serve() {}\n").unwrap();
        fs::write(
            root.join("src/mcp/mod.rs"),
            "pub fn serve() {}\nfn other() { serve(); }\n",
        )
        .unwrap();

        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        let all = index.references_with_meta("serve").unwrap();
        assert!(!all.results.is_empty());

        let mcp_only = index
            .references_with_meta_opts("serve", Some("crate::mcp"))
            .unwrap();
        // Every retained ref must resolve toward crate::mcp (same-module or import).
        assert!(
            !mcp_only.results.is_empty(),
            "expected at least the same-module call in mcp"
        );
        assert!(
            mcp_only.confidence == Confidence::High || mcp_only.confidence == Confidence::Medium,
            "narrowed refs should not be low-confidence theater; got {:?}",
            mcp_only.confidence
        );
        // api-only seed should not include mcp-internal same-module call if filtered.
        let api_only = index
            .references_with_meta_opts("serve", Some("crate::api"))
            .unwrap();
        assert!(
            api_only.results.len() < all.results.len()
                || api_only.results.is_empty()
                || mcp_only.results != api_only.results,
            "module filter should change the reference set"
        );
    }

    #[test]
    fn impact_with_module_is_candidate_medium_not_fake_high() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("src/lib.rs"),
            "fn a() {}\nfn b() { a(); }\nfn c() { b(); }\n",
        )
        .unwrap();

        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        let meta = index.impact_with_meta("a").unwrap();
        assert!(!meta.results.is_empty());
        assert_eq!(meta.confidence, Confidence::Medium);
        assert!(meta.notes.iter().any(|n| n.contains("candidate blast radius")));
    }

    #[test]
    fn impact_qualified_does_not_expand_other_same_name_def() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src/api")).unwrap();
        fs::create_dir_all(root.join("src/mcp")).unwrap();
        fs::write(root.join("src/lib.rs"), "mod api;\nmod mcp;\n").unwrap();
        fs::write(root.join("src/api/mod.rs"), "pub fn serve() {}\n").unwrap();
        fs::write(
            root.join("src/mcp/mod.rs"),
            "pub fn serve() {}\nfn other() { serve(); }\n",
        )
        .unwrap();

        let mut index = Index::open_in_memory().unwrap();
        index.index_path(root).unwrap();

        let api_impact = index
            .impact_with_meta_opts("serve", Some("crate::api"))
            .unwrap();
        assert!(
            api_impact.results.is_empty(),
            "api::serve is unused; got {:?}",
            api_impact.results.iter().map(|s| &s.name).collect::<Vec<_>>()
        );

        let mcp_impact = index.impact_with_meta("crate::mcp::serve").unwrap();
        assert!(
            mcp_impact.results.iter().any(|s| s.name == "other"),
            "mcp::serve should impact same-module other; got {:?}",
            mcp_impact.results.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        assert_eq!(mcp_impact.confidence, Confidence::Medium);
        // Bare-name impact would seed both defs; qualified must not pull api-only noise
        // and must not report High.
        assert_ne!(mcp_impact.confidence, Confidence::High);
    }
}
