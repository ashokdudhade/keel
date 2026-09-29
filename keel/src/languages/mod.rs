//! Language plugin trait and registry. The core dispatches to plugins by file
//! extension without knowing any language specifics.

pub mod go;
pub mod javascript;
pub mod python;
pub mod rust;
pub mod typescript;

use crate::error::Result;
use crate::graph::types::{Import, ImplRecord, Reference, Symbol};
use std::path::{Path, PathBuf};
use tree_sitter::Node;

/// Path-based module identity: extension stripped, `/` separators.
///
/// Used by TypeScript (always) and Go (`package main`) so same-named symbols in
/// different files do not collide.
pub fn path_module_identity(path: &Path) -> String {
    path.with_extension("")
        .to_string_lossy()
        .replace('\\', "/")
}

/// Resolve a JS/TS-style relative specifier (`./x`, `../y`) against `from_file`
/// into the same identity [`path_module_identity`] would assign the target file.
///
/// Non-relative specifiers (packages, absolute URLs) are returned unchanged.
/// Specifiers may include a `::export` suffix (named import); only the path
/// portion is resolved.
pub fn resolve_relative_path_module(from_file: &Path, specifier: &str) -> String {
    let (path_part, suffix) = match specifier.split_once("::") {
        Some((p, rest)) => (p, Some(rest)),
        None => (specifier, None),
    };
    let resolved = if path_part.starts_with("./") || path_part.starts_with("../") {
        let base = from_file.parent().unwrap_or_else(|| Path::new(""));
        let joined = base.join(path_part);
        path_module_identity(&normalize_lexically(&joined))
    } else {
        path_part.to_string()
    };
    match suffix {
        Some(rest) => format!("{resolved}::{rest}"),
        None => resolved,
    }
}

/// Collapse `.` / `..` without touching the filesystem.
fn normalize_lexically(path: &Path) -> PathBuf {
    use std::path::{Component, PathBuf};
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(s) => out.push(s),
            Component::RootDir => out.push(Component::RootDir.as_os_str()),
            Component::Prefix(p) => out.push(p.as_os_str()),
        }
    }
    out
}

/// Stable file-path string for empty-scope reference containers.
pub fn file_path_key(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Maximum tree depth the extraction walks descend. Real files nest tens
/// of levels (deepest observed across six real repos: 89); beyond this
/// the file errors loudly instead of overflowing a worker-thread stack.
pub(crate) const MAX_WALK_DEPTH: u32 = 1024;

/// Tracks recursion depth for one extraction walk; see [`DepthGuard`].
///
/// Interior mutability (`Cell`) so walks hold only shared borrows: the
/// guard stays alive across recursive calls without borrow conflicts.
pub(crate) struct WalkBudget {
    depth: std::cell::Cell<u32>,
}

impl WalkBudget {
    pub(crate) fn new() -> Self {
        Self {
            depth: std::cell::Cell::new(0),
        }
    }
}

/// RAII depth guard: [`DepthGuard::enter`] fails once [`MAX_WALK_DEPTH`]
/// is exceeded (the caller errors the file), else holds one level until
/// dropped so early returns and `?` stay balanced.
pub(crate) struct DepthGuard<'a> {
    budget: &'a WalkBudget,
}

impl<'a> DepthGuard<'a> {
    pub(crate) fn enter(budget: &'a WalkBudget) -> Option<Self> {
        if budget.depth.get() >= MAX_WALK_DEPTH {
            return None;
        }
        budget.depth.set(budget.depth.get() + 1);
        Some(Self { budget })
    }
}

impl Drop for DepthGuard<'_> {
    fn drop(&mut self) {
        self.budget.depth.set(self.budget.depth.get() - 1);
    }
}

/// Whether a JS/TS declaration node sits at module top level: a direct
/// child of `program`, possibly through `export_statement` /
/// `ambient_declaration` wrappers (`export const x`, `declare const x`).
/// Plain variables gate on this — function bodies are full of locals that
/// must not become module symbols — while bound arrow functions stay
/// ungated (existing lookup behavior).
pub(crate) fn is_top_level_js_declaration(node: Node) -> bool {
    let mut current = node;
    loop {
        let Some(parent) = current.parent() else {
            return false;
        };
        match parent.kind() {
            "program" => return true,
            "export_statement" | "ambient_declaration" => current = parent,
            _ => return false,
        }
    }
}

/// Module identity for a file that defines no symbols (a top-level-only
/// script or barrel), derived with the same function the extractor uses
/// so the identity matches symbol modules exactly. Rust uses its
/// top-scope `crate::` identity; Go has no path-derived identity (the
/// package clause lives in source) and yields `None`.
pub(crate) fn path_module_fallback(path: &Path) -> Option<String> {
    match path.extension().and_then(|s| s.to_str()) {
        Some("py" | "pyi") => Some(python::python_module_identity(path)),
        Some("js" | "mjs" | "cjs" | "jsx" | "ts" | "mts" | "cts" | "tsx") => {
            Some(path_module_identity(path))
        }
        Some("rs") => Some(rust::rust_file_module_identity(path)),
        _ => None,
    }
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

    /// Whether `source_code` parses with syntax errors. Tree-sitter still
    /// produces a (partial) tree, so extraction continues — but the indexer
    /// warns per file and bumps the `syntax_errors` count on
    /// [`crate::index::IndexStats`], so agents know results for that file may
    /// be incomplete. Defaults to `false` so third-party plugins keep working
    /// unmodified.
    fn has_syntax_errors(&self, source_code: &str) -> bool {
        let _ = source_code;
        false
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

    /// A registry with all built-in plugins (Rust, TypeScript/TSX, Go,
    /// JavaScript/JSX, Python).
    pub fn with_defaults() -> Self {
        let mut registry = Self::empty();
        registry.register(Box::new(rust::RustPlugin));
        registry.register(Box::new(go::GoPlugin));
        typescript::register(&mut registry.plugins);
        javascript::register(&mut registry.plugins);
        registry.register(Box::new(python::PythonPlugin));
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
                    container: String::new(),
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

    #[test]
    fn resolve_relative_path_module_against_importer() {
        let from = Path::new("src/auth/service.ts");
        assert_eq!(
            resolve_relative_path_module(from, "./util"),
            "src/auth/util"
        );
        assert_eq!(
            resolve_relative_path_module(from, "../api/token::Token"),
            "src/api/token::Token"
        );
        assert_eq!(
            resolve_relative_path_module(from, "lodash"),
            "lodash"
        );
    }
}
