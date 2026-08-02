//! Common-path trust fixtures across languages.

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
    assert!(deps
        .results
        .iter()
        .any(|d| d.module_path.starts_with("crate::api")));

    let def = index.definition_with_meta("serve").unwrap();
    assert_eq!(def.results[0].module_path, "crate::mcp");
    assert_eq!(def.confidence, Confidence::High);
}

#[test]
fn typescript_relative_import_dependencies() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src/api")).unwrap();
    fs::write(
        root.join("src/api/token.ts"),
        "export class Token {}\n",
    )
    .unwrap();
    fs::write(
        root.join("src/service.ts"),
        "import { Token } from './api/token';\nexport function serve() { return Token; }\n",
    )
    .unwrap();

    let mut index = Index::open_in_memory().unwrap();
    index.index_path(root).unwrap();

    let deps = index.dependencies_with_meta("src/service").unwrap();
    assert!(
        deps.results.iter().any(|d| d.module_path == "src/api/token"),
        "deps={:?}",
        deps.results
    );

    let callers = index.callers_with_meta("Token").unwrap();
    assert!(!callers.results.is_empty() || deps.confidence == Confidence::High);
}

#[test]
fn javascript_relative_import_dependencies() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src/api")).unwrap();
    fs::write(root.join("src/api/token.js"), "export class Token {}\n").unwrap();
    fs::write(
        root.join("src/service.js"),
        "import { Token } from './api/token';\nexport function serve() {}\n",
    )
    .unwrap();

    let mut index = Index::open_in_memory().unwrap();
    index.index_path(root).unwrap();

    let deps = index.dependencies_with_meta("src/service").unwrap();
    assert!(
        deps.results.iter().any(|d| d.module_path == "src/api/token"),
        "deps={:?}",
        deps.results
    );
}

#[test]
fn python_relative_import_dependencies() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("pkg/auth")).unwrap();
    fs::write(root.join("pkg/auth/util.py"), "def helper():\n    pass\n").unwrap();
    fs::write(
        root.join("pkg/auth/service.py"),
        "from .util import helper\n\ndef serve():\n    helper()\n",
    )
    .unwrap();

    let mut index = Index::open_in_memory().unwrap();
    index.index_path(root).unwrap();

    let deps = index
        .dependencies_with_meta("pkg.auth.service")
        .unwrap();
    assert!(
        deps
            .results
            .iter()
            .any(|d| d.module_path == "pkg.auth.util" || d.module_path.starts_with("pkg.auth.util")),
        "deps={:?}",
        deps.results
    );
}

#[test]
fn go_import_path_reaches_package_name() {
    let dir = tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("helper")).unwrap();
    fs::create_dir_all(root.join("auth")).unwrap();
    fs::write(
        root.join("helper/helper.go"),
        "package helper\n\nfunc Assist() {}\n",
    )
    .unwrap();
    fs::write(
        root.join("auth/auth.go"),
        "package auth\n\nimport \"example.com/app/helper\"\n\nfunc Login() { helper.Assist() }\n",
    )
    .unwrap();

    let mut index = Index::open_in_memory().unwrap();
    index.index_path(root).unwrap();

    let deps = index.dependencies_with_meta("auth").unwrap();
    assert!(
        deps
            .results
            .iter()
            .any(|d| d.module_path.contains("helper")),
        "deps={:?}",
        deps.results
    );

    let callers = index.callers_with_meta("Assist").unwrap();
    assert!(
        !callers.results.is_empty(),
        "expected callers of Assist, notes={:?}",
        callers.notes
    );
}
