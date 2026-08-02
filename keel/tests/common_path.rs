//! Common-path trust fixtures (Rust first; other languages follow).

use keel::{Confidence, Index};
use std::fs;
use tempfile::tempdir;

#[test]
fn rust_nested_module_dependencies_and_meta() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src/mcp")).unwrap();
    fs::create_dir_all(root.join("src/api")).unwrap();
    fs::write(root.join("src/lib.rs"), "mod api;\nmod mcp;\n").unwrap();
    fs::write(root.join("src/api/mod.rs"), "pub struct Token;\n").unwrap();
    fs::write(
        root.join("src/mcp/mod.rs"),
        "use crate::api::Token;\npub fn serve() { let _ = Token; }\n",
    )
    .unwrap();

    let mut index = Index::open_in_memory().unwrap();
    index.index_path(root).unwrap();

    let deps = index.dependencies_with_meta("crate::mcp").unwrap();
    assert!(!deps.results.is_empty());
    assert_eq!(deps.confidence, Confidence::High);
    assert!(deps.results.iter().any(|d| d.module_path.starts_with("crate::api")));

    let def = index.definition_with_meta("serve").unwrap();
    assert_eq!(def.results[0].module_path, "crate::mcp");
    assert_eq!(def.confidence, Confidence::High);
}
