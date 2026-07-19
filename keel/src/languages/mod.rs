//! Language plugin trait and registry. The core dispatches to plugins by file
//! extension without knowing any language specifics.

pub mod go;
pub mod rust;
pub mod typescript;

use crate::error::Result;
use crate::graph::types::{Import, ImplRecord, Reference, Symbol};
use std::path::Path;

/// Path-based module identity: extension stripped, `/` separators.
///
/// Used by TypeScript (always) and Go (`package main`) so same-named symbols in
/// different files do not collide.
pub fn path_module_identity(path: &Path) -> String {
    path.with_extension("")
        .to_string_lossy()
        .replace('\\', "/")
}

/// Stable file-path string for empty-scope reference containers.
pub fn file_path_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// A language-specific extractor. Must be `Sync` so plugins can be shared across
/// Rayon worker threads during parallel indexing.
pub trait LanguagePlugin: Sync {
    /// File extensions (without dot) this plugin handles, e.g. `["rs"]`.
    fn extensions(&self) -> &[&str];

    /// Extract defined symbols from source. Returned symbols have an empty `file`.
    fn extract_symbols(&self, path: &Path, source_code: &str) -> Result<Vec<Symbol>>;

    /// Extract references (call/macro sites) from source. Returned references have
    /// an empty `file`.
    fn extract_references(&self, path: &Path, source_code: &str) -> Result<Vec<Reference>>;

    /// Extract `use`/import records from source. Returned imports have an empty
    /// `file`. Defaults to none so plugins can adopt this incrementally.
    fn extract_imports(&self, path: &Path, source_code: &str) -> Result<Vec<Import>> {
        let _ = (path, source_code);
        Ok(vec![])
    }

    /// Extract `impl` block records from source. Returned records have an empty
    /// `file`. Defaults to none so plugins can adopt this incrementally.
    fn extract_impls(&self, path: &Path, source_code: &str) -> Result<Vec<ImplRecord>> {
        let _ = (path, source_code);
        Ok(vec![])
    }
}

/// Holds the set of available language plugins.
pub struct Registry {
    plugins: Vec<Box<dyn LanguagePlugin>>,
}

impl Registry {
    /// An empty registry with no plugins (for community/custom registration).
    pub fn empty() -> Self {
        Registry {
            plugins: Vec::new(),
        }
    }

    /// A registry with all built-in plugins (Rust, TypeScript/TSX, Go).
    pub fn with_defaults() -> Self {
        let mut registry = Self::empty();
        registry.register(Box::new(rust::RustPlugin));
        registry.register(Box::new(go::GoPlugin));
        typescript::register(&mut registry.plugins);
        registry
    }

    /// Register a language plugin.
    ///
    /// When multiple plugins claim the same extension, [`Registry::for_extension`]
    /// returns the first one registered.
    pub fn register(&mut self, plugin: Box<dyn LanguagePlugin>) {
        self.plugins.push(plugin);
    }

    /// Every extension claimed by a registered plugin (deduplicated, unsorted).
    pub fn extensions(&self) -> Vec<&str> {
        let mut out = Vec::new();
        for plugin in &self.plugins {
            for ext in plugin.extensions() {
                if !out.contains(ext) {
                    out.push(*ext);
                }
            }
        }
        out
    }

    /// The first plugin registered for `ext`, if any.
    pub fn for_extension(&self, ext: &str) -> Option<&dyn LanguagePlugin> {
        self.plugins
            .iter()
            .map(|b| b.as_ref())
            .find(|p| p.extensions().contains(&ext))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::types::SymbolKind;
    use crate::index;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::tempdir;

    /// Tiny fake plugin for the community registration surface.
    struct ToyPlugin;

    impl LanguagePlugin for ToyPlugin {
        fn extensions(&self) -> &[&str] {
            &["toy"]
        }

        fn extract_symbols(&self, _path: &Path, source_code: &str) -> Result<Vec<Symbol>> {
            // One symbol per non-empty line: `symbol <name>`.
            let mut out = Vec::new();
            for (i, line) in source_code.lines().enumerate() {
                let Some(name) = line.strip_prefix("symbol ") else {
                    continue;
                };
                let name = name.trim();
                if name.is_empty() {
                    continue;
                }
                out.push(Symbol {
                    name: name.to_string(),
                    kind: SymbolKind::Other("toy".into()),
                    file: PathBuf::new(),
                    start_line: (i as u32) + 1,
                    start_col: 1,
                    module_path: "toy".into(),
                });
            }
            Ok(out)
        }

        fn extract_references(&self, _path: &Path, _source_code: &str) -> Result<Vec<Reference>> {
            Ok(vec![])
        }
    }

    #[test]
    fn register_custom_toy_plugin_indexes_extension() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("demo.toy"), "symbol Widget\n").unwrap();

        let mut registry = Registry::empty();
        registry.register(Box::new(ToyPlugin));

        let mut conn = rusqlite::Connection::open_in_memory().unwrap();
        let stats = index::index_repository_with(root, &mut conn, &registry).unwrap();
        assert_eq!(stats.indexed, 1);

        let defs = crate::db::queries::find_definition(&conn, "Widget").unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].kind, SymbolKind::Other("toy".into()));
        assert_eq!(defs[0].module_path, "toy");
    }

    #[test]
    fn empty_registry_has_no_extensions() {
        assert!(Registry::empty().extensions().is_empty());
    }

    #[test]
    fn path_module_identity_strips_extension() {
        assert_eq!(
            path_module_identity(Path::new("src/auth/service.ts")),
            "src/auth/service"
        );
    }
}
