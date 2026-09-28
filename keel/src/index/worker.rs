//! Parallel file parsing pipeline. CPU-bound parse/extract runs in parallel;
//! database writes are serialized by the orchestrator in `index::index_repository`.

use crate::error::{Result, KeelError};
use crate::graph::types::{FileNode, ImplRecord, Import, Reference, Symbol};
use crate::languages::Registry;
use ignore::WalkBuilder;
use rayon::prelude::*;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
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
    /// Whether the source parsed with syntax errors (extraction above is
    /// partial; the indexer warns and counts the file separately).
    pub syntax_error: bool,
}

/// Store paths relative to the index `root` (forward-slash normalized keys come
/// from [`PathBuf`]'s platform display; callers use the returned path as the DB key).
pub fn normalize_path(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root)
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf())
}

/// Directory names that never hold hand-written source: dependency vendors,
/// build outputs, and tool caches. Pruned even when no `.gitignore` covers
/// them (fresh checkouts, ad-hoc `npm install`), so vendored copies cannot
/// pollute definitions or references. Deliberately excludes `build/` and
/// `out/`, which sometimes hold hand-written build scripts.
const SKIPPED_DIRS: &[&str] = &[
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

/// Collect all source files under `root` whose extension has a registered
/// plugin, honoring `.gitignore` plus Keel's own `.keelignore` (same
/// gitignore syntax, for generated/checked-in code worth skipping without
/// touching version control) and pruning [`SKIPPED_DIRS`].
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
        // Keel-only exclusions (generated code, fixtures) in the same syntax.
        .add_custom_ignore_filename(".keelignore")
        // Prune junk dirs before descending (perf) — `filter_entry(false)`
        // skips the whole subtree. Depth 0 is the walk root itself: never
        // prune it, so a project rooted in `target/` still indexes.
        .filter_entry(|entry| {
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
        })
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
/// [`FileNode`]. A leading UTF-8 BOM is blanked to spaces (same 3 bytes,
/// so every byte offset — and every reported line/column — is unchanged);
/// grammars reject the raw BOM as a syntax error even though the file is
/// otherwise valid.
pub fn parse_file_contents(
    rel_path: &Path,
    source: &str,
    content_hash: String,
    registry: &Registry,
) -> Result<ParsedFile> {
    let ext = rel_path.extension().and_then(|s| s.to_str()).unwrap_or("");
    let plugin = registry
        .for_extension(ext)
        .ok_or_else(|| KeelError::UnsupportedExtension(ext.to_string()))?;
    let scrubbed;
    let source = match source.strip_prefix('\u{FEFF}') {
        Some(rest) => {
            scrubbed = format!("   {rest}");
            scrubbed.as_str()
        }
        None => source,
    };

    let symbols = plugin.extract_symbols(rel_path, source)?;
    let references = plugin.extract_references(rel_path, source)?;
    let imports = plugin.extract_imports(rel_path, source)?;
    let impls = plugin.extract_impls(rel_path, source)?;
    let syntax_error = plugin.has_syntax_errors(source);

    Ok(ParsedFile {
        node: FileNode {
            path: rel_path.to_path_buf(),
            content_hash,
        },
        symbols,
        references,
        imports,
        impls,
        syntax_error,
    })
}

/// Read, parse, and extract a single file (absolute `abs_path`, stored as `rel_path`).
pub fn parse_file(abs_path: &Path, rel_path: &Path, registry: &Registry) -> Result<ParsedFile> {
    let bytes = fs::read(abs_path).map_err(|source| KeelError::Io {
        path: abs_path.to_path_buf(),
        source,
    })?;
    let content_hash = hex::encode(Sha256::digest(&bytes));
    let source = std::str::from_utf8(&bytes).map_err(|_| KeelError::Parse)?;
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
        /// Content hash when the bytes were hashed before failing
        /// (UTF-8/parse failures). `None` when the file could not be
        /// read at all, so transient I/O errors retry every pass.
        /// A recorded hash lets later passes skip the unchanged file
        /// instead of re-reporting the same error on every query.
        hash: Option<String>,
    },
}

/// Owning-crate names for an index run: crate directory → crate name with
/// `-` normalized to `_` (as in `use` paths). Each `.rs` file resolves to
/// its nearest ancestor `Cargo.toml` (bounded by `root`); manifests are
/// read once per directory, and files outside any crate are absent.
///
/// [`HashMap`] is empty for non-Rust projects; lookups stay cheap.
pub fn rust_crate_names(root: &Path, files: &[PathBuf]) -> HashMap<PathBuf, String> {
    let mut out = HashMap::new();
    let mut scanned: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for file in files {
        if file.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let Some(parent) = file.parent() else {
            continue;
        };
        for anc in parent.ancestors() {
            if !anc.starts_with(root) {
                break;
            }
            if out.contains_key(anc) {
                break;
            }
            if !scanned.insert(anc.to_path_buf()) {
                continue;
            }
            let manifest = anc.join("Cargo.toml");
            if manifest.is_file() {
                // Nearest manifest wins either way: a manifest without a
                // crate name (workspace root) means no owning crate here.
                if let Ok(text) = fs::read_to_string(&manifest) {
                    if let Some(name) = parse_cargo_crate_name(&text) {
                        out.insert(anc.to_path_buf(), name);
                    }
                }
                break;
            }
        }
    }
    out
}

/// Owning crate name for `abs_path` from a [`rust_crate_names`] map: the
/// nearest ancestor directory (bounded by `root`) present in the map.
pub fn owning_crate<'m>(
    crates: &'m HashMap<PathBuf, String>,
    root: &Path,
    abs_path: &Path,
) -> Option<&'m str> {
    if crates.is_empty() {
        return None;
    }
    let parent = abs_path.parent()?;
    for anc in parent.ancestors() {
        if !anc.starts_with(root) {
            break;
        }
        if let Some(name) = crates.get(anc) {
            return Some(name);
        }
    }
    None
}

/// Crate name from manifest text: `[lib] name` when renamed, else
/// `[package] name`, with `-` normalized to `_`. Workspace roots (no
/// `[package]`) yield `None`.
fn parse_cargo_crate_name(text: &str) -> Option<String> {
    let mut section = "";
    let mut package: Option<String> = None;
    let mut lib: Option<String> = None;
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim();
            // `[package]` only; `[package.metadata.*]` is not the package.
            continue;
        }
        if section != "package" && section != "lib" {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key.trim() != "name" {
            continue;
        }
        let name = value.trim().trim_matches(|c| c == '"' || c == '\'').replace('-', "_");
        if name.is_empty() {
            continue;
        }
        if section == "lib" {
            lib = Some(name);
        } else {
            package = Some(name);
        }
    }
    lib.or(package)
}

/// Rewrite same-crate `use` paths to `crate::` roots: `use m::store::x`
/// inside crate `m` means `crate::store::x`, matching the `crate::`-rooted
/// definition modules. Imports rooted anywhere else (external crates,
/// `crate`/`self`/`super`) pass through untouched.
fn normalize_crate_imports(imports: &mut [Import], crate_name: &str) {
    if crate_name.is_empty() {
        return;
    }
    let prefix = format!("{crate_name}::");
    for import in imports.iter_mut() {
        if let Some(rest) = import.module_path.strip_prefix(&prefix) {
            import.module_path = format!("crate::{rest}");
        }
    }
}

/// Hash every file; parse those whose hash changed. Reads each file at most once.
///
/// `crates` is the [`rust_crate_names`] map for same-crate `use` normalization.
///
/// Failures for individual files become [`FileOutcome::Failed`] rather than
/// aborting the whole batch.
pub fn hash_and_parse(
    root: &Path,
    files: &[PathBuf],
    existing: &std::collections::HashMap<String, String>,
    registry: &Registry,
    crates: &HashMap<PathBuf, String>,
) -> Vec<FileOutcome> {
    files
        .par_iter()
        .map(|abs_path| {
            let rel = normalize_path(root, abs_path);
            let key = rel.to_string_lossy().into_owned();
            match process_one(abs_path, &rel, &key, existing, registry, root, crates) {
                Ok(outcome) => outcome,
                Err(e) => FileOutcome::Failed {
                    path: abs_path.clone(),
                    message: e.to_string(),
                    hash: None,
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
    root: &Path,
    crates: &HashMap<PathBuf, String>,
) -> Result<FileOutcome> {
    let bytes = fs::read(abs_path).map_err(|source| KeelError::Io {
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
                hash: Some(hash),
            });
        }
    };

    match parse_file_contents(rel, source, hash.clone(), registry) {
        Ok(mut parsed) => {
            if rel.extension().and_then(|s| s.to_str()) == Some("rs") {
                if let Some(crate_name) = owning_crate(crates, root, abs_path) {
                    normalize_crate_imports(&mut parsed.imports, crate_name);
                }
            }
            Ok(FileOutcome::Parsed(parsed))
        }
        Err(e) => Ok(FileOutcome::Failed {
            path: abs_path.to_path_buf(),
            message: e.to_string(),
            hash: Some(hash),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_names_prefer_lib_override_and_underscores() {
        assert_eq!(
            parse_cargo_crate_name("[package]\nname = \"my-crate\"\nversion = \"0.1.0\"\n"),
            Some("my_crate".to_string())
        );
        assert_eq!(
            parse_cargo_crate_name("[package]\nname = \"pkg\"\n[lib]\nname = \"renamed\"\n"),
            Some("renamed".to_string())
        );
        // Workspace roots and name-less manifests yield nothing.
        assert_eq!(parse_cargo_crate_name("[workspace]\n"), None);
        assert_eq!(parse_cargo_crate_name(""), None);
        // `[package.metadata]` must not satisfy `[package]`.
        assert_eq!(
            parse_cargo_crate_name("[package.metadata.docs]\nname = \"nope\"\n"),
            None
        );
    }

    #[test]
    fn same_crate_imports_rewrite_others_pass_through() {
        let mk = |module_path: &str, alias: Option<&str>| Import {
            module_path: module_path.into(),
            alias: alias.map(str::to_string),
            file: PathBuf::new(),
        };
        let mut imports = vec![
            mk("m::store::Mem", Some("Store")),
            mk("serde::de::X", None),
            mk("crate::local::Y", None),
        ];
        normalize_crate_imports(&mut imports, "m");
        let paths: Vec<&str> = imports.iter().map(|i| i.module_path.as_str()).collect();
        assert_eq!(paths, vec!["crate::store::Mem", "serde::de::X", "crate::local::Y"]);
    }

    #[test]
    fn bom_prefixed_files_parse_clean_with_exact_positions() {
        let registry = Registry::with_defaults();
        let source = "\u{FEFF}export const bom = 1;\nexport function useBom() {\n  return bom + 1;\n}\n";
        let parsed =
            parse_file_contents(Path::new("b.ts"), source, "hash".to_string(), &registry).unwrap();
        assert!(!parsed.syntax_error);
        let names: Vec<&str> = parsed.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"bom"), "symbols: {names:?}");
        assert!(names.contains(&"useBom"), "symbols: {names:?}");
        // Line-1 columns are raw-file exact: 3 BOM bytes + 13 chars
        // of `export const ` put `bom` at byte 16, column 17.
        let sym = parsed.symbols.iter().find(|s| s.name == "bom").unwrap();
        assert_eq!((sym.start_line, sym.start_col), (1, 17));
        assert!(parsed.references.iter().any(|r| r.name == "bom" && r.start_line == 3));
    }
}
