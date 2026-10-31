//! Filesystem traversal seam: one walker configuration over the `ignore`
//! crate, so gitignore/keelignore semantics and junk-dir pruning live in
//! exactly one place. Indexing, drift, and ADR discovery all build on it.

use ignore::{DirEntry, WalkBuilder};
use std::path::Path;

/// Directory names that never hold hand-written source: dependency vendors,
/// build outputs, and tool caches. Pruned even when no `.gitignore` covers
/// them (fresh checkouts, ad-hoc `npm install`), so vendored copies cannot
/// pollute definitions or references. Deliberately excludes `build/` and
/// `out/`, which sometimes hold hand-written build scripts.
pub(crate) const SKIPPED_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    ".eggs",
    ".mypy_cache",
    ".next",
    ".nox",
    ".nuxt",
    ".pytest_cache",
    ".ruff_cache",
    ".tox",
    ".venv",
    "__pycache__",
    "dist",
    "node_modules",
    "target",
    "vendor",
    "venv",
];

/// Base walker: standard filters on, `.gitignore` honored even when `root`
/// is not inside a git repository (by default the `ignore` crate only
/// applies gitignore rules when a `.git` dir is present), plus Keel-only
/// exclusions in the same syntax via `.keelignore`. Callers add their own
/// `filter_entry` pruning and file filters on top.
pub(crate) fn walker(root: &Path) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder.standard_filters(true);
    builder.require_git(false);
    builder.add_custom_ignore_filename(".keelignore");
    builder
}

/// `filter_entry` predicate pruning [`SKIPPED_DIRS`] before descending
/// (perf: `false` skips the whole subtree). Depth 0 is the walk root
/// itself: never prune it, so a project rooted in `target/` still walks.
pub(crate) fn prune_skipped_dirs(entry: &DirEntry) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
        return true;
    }
    entry
        .file_name()
        .to_str()
        .is_none_or(|name| !SKIPPED_DIRS.contains(&name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walker_honors_keelignore_and_prunes_skipped_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("target/a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(root.join("src/b.rs"), "fn b() {}\n").unwrap();
        std::fs::write(root.join("src/c.rs"), "fn c() {}\n").unwrap();
        std::fs::write(root.join(".keelignore"), "src/c.rs\n").unwrap();

        let mut found: Vec<String> = walker(root)
            .filter_entry(prune_skipped_dirs)
            .build()
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_type().map(|t| t.is_file()).unwrap_or(false))
            .map(|entry| {
                entry
                    .path()
                    .strip_prefix(root)
                    .unwrap_or(entry.path())
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        found.sort();
        // `.keelignore` itself is hidden-filtered; `src/c.rs` is keelignored
        // and `target/a.rs` is pruned.
        assert_eq!(found, vec!["src/b.rs".to_string()]);
    }
}
