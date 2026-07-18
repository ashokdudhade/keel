//! Language plugin trait and registry. The core dispatches to plugins by file
//! extension without knowing any language specifics.

pub mod rust;

use crate::error::Result;
use crate::graph::types::{Reference, Symbol};

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
}

/// Holds the set of available language plugins.
pub struct Registry {
    plugins: Vec<Box<dyn LanguagePlugin>>,
}

impl Registry {
    /// A registry with all built-in plugins (Rust in v0.1).
    pub fn with_defaults() -> Self {
        Registry { plugins: vec![Box::new(rust::RustPlugin)] }
    }

    /// The first plugin registered for `ext`, if any.
    pub fn for_extension(&self, ext: &str) -> Option<&dyn LanguagePlugin> {
        self.plugins
            .iter()
            .map(|b| b.as_ref())
            .find(|p| p.extensions().contains(&ext))
    }
}
