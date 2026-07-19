//! Language plugin trait and registry. The core dispatches to plugins by file
//! extension without knowing any language specifics.

pub mod rust;
pub mod typescript;

use crate::error::Result;
use crate::graph::types::{Import, ImplRecord, Reference, Symbol};

/// A language-specific extractor. Must be `Sync` so plugins can be shared across
/// Rayon worker threads during parallel indexing.
pub trait LanguagePlugin: Sync {
    /// File extensions (without dot) this plugin handles, e.g. `["rs"]`.
    fn extensions(&self) -> &[&str];

    /// Extract defined symbols from source. Returned symbols have an empty `file`.
    fn extract_symbols(&self, source_code: &str) -> Result<Vec<Symbol>>;

    /// Extract references (call/macro sites) from source. Returned references have
    /// an empty `file`.
    fn extract_references(&self, source_code: &str) -> Result<Vec<Reference>>;

    /// Extract `use`/import records from source. Returned imports have an empty
    /// `file`. Defaults to none so plugins can adopt this incrementally.
    fn extract_imports(&self, source_code: &str) -> Result<Vec<Import>> {
        let _ = source_code;
        Ok(vec![])
    }

    /// Extract `impl` block records from source. Returned records have an empty
    /// `file`. Defaults to none so plugins can adopt this incrementally.
    fn extract_impls(&self, source_code: &str) -> Result<Vec<ImplRecord>> {
        let _ = source_code;
        Ok(vec![])
    }
}

/// Holds the set of available language plugins.
pub struct Registry {
    plugins: Vec<Box<dyn LanguagePlugin>>,
}

impl Registry {
    /// A registry with all built-in plugins (Rust, TypeScript/TSX).
    pub fn with_defaults() -> Self {
        let mut plugins: Vec<Box<dyn LanguagePlugin>> = vec![Box::new(rust::RustPlugin)];
        typescript::register(&mut plugins);
        Registry { plugins }
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
