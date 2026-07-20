//! Per-language end-to-end index + query coverage for every built-in language.

use keel::db::{queries, schema};
use keel::graph::types::SymbolKind;
use keel::index;
use rusqlite::Connection;
use std::fs;
use std::path::Path;

fn index_one(root: &Path) -> Connection {
    let mut conn = Connection::open_in_memory().unwrap();
    schema::initialize(&conn).unwrap();
    let stats = index::index_repository(root, &mut conn).unwrap();
    assert!(stats.indexed >= 1, "expected at least one indexed file");
    conn
}

#[test]
fn language_rust_definition_and_callers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub struct AuthService;\npub fn create_order() {}\nfn caller() { create_order(); }\n",
    )
    .unwrap();

    let conn = index_one(root);
    let defs = queries::find_definition(&conn, "AuthService").unwrap();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].kind, SymbolKind::Struct);

    let callers = queries::find_references(&conn, "create_order").unwrap();
    assert!(
        callers.iter().any(|r| r.name == "create_order"),
        "expected create_order call site: {callers:?}"
    );
}

#[test]
fn language_typescript_definition_and_references() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/auth.ts"),
        "export class AuthService { login(): void { this.refresh(); } refresh(): void {} }\nexport function createOrder(): void {}\nfunction run(): void { createOrder(); }\n",
    )
    .unwrap();

    let conn = index_one(root);
    assert_eq!(
        queries::find_definition(&conn, "AuthService").unwrap()[0].kind,
        SymbolKind::Struct
    );
    let refs = queries::find_references(&conn, "createOrder").unwrap();
    assert!(!refs.is_empty());
}

#[test]
fn language_go_definition_and_module_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("auth")).unwrap();
    fs::write(
        root.join("auth/service.go"),
        "package auth\n\ntype AuthService struct{}\n\nfunc CreateOrder() {}\nfunc Run() { CreateOrder() }\n",
    )
    .unwrap();

    let conn = index_one(root);
    let defs = queries::find_definition(&conn, "AuthService").unwrap();
    assert_eq!(defs[0].module_path, "auth");
    assert!(!queries::find_references(&conn, "CreateOrder")
        .unwrap()
        .is_empty());
}

#[test]
fn language_javascript_definition_and_imports() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/auth.js"),
        "import { helper } from './util';\nexport class AuthService { login() { helper(); } }\nexport function createOrder() {}\nfunction run() { createOrder(); }\n",
    )
    .unwrap();
    fs::write(root.join("src/util.js"), "export function helper() {}\n").unwrap();

    let mut conn = Connection::open_in_memory().unwrap();
    schema::initialize(&conn).unwrap();
    let stats = index::index_repository(root, &mut conn).unwrap();
    assert_eq!(stats.indexed, 2);

    assert_eq!(
        queries::find_definition(&conn, "AuthService").unwrap()[0].kind,
        SymbolKind::Struct
    );
    assert!(!queries::find_references(&conn, "createOrder")
        .unwrap()
        .is_empty());
}

#[test]
fn language_jsx_definition() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/Widget.jsx"),
        "export function Widget() { return <button/>; }\n",
    )
    .unwrap();

    let conn = index_one(root);
    let defs = queries::find_definition(&conn, "Widget").unwrap();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].kind, SymbolKind::Function);
    assert_eq!(defs[0].file.as_os_str(), "src/Widget.jsx");
}

#[test]
fn language_python_definition_and_references() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("pkg")).unwrap();
    fs::write(
        root.join("pkg/auth.py"),
        "class AuthService:\n    def login(self):\n        self.refresh()\n    def refresh(self):\n        pass\n\ndef create_order():\n    pass\n\ndef run():\n    create_order()\n",
    )
    .unwrap();

    let conn = index_one(root);
    let defs = queries::find_definition(&conn, "AuthService").unwrap();
    assert_eq!(defs[0].module_path, "pkg.auth");
    assert!(!queries::find_references(&conn, "create_order")
        .unwrap()
        .is_empty());
}

#[test]
fn language_python_stub_definition() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("pkg")).unwrap();
    fs::write(root.join("pkg/types.pyi"), "class Config: ...\n").unwrap();

    let conn = index_one(root);
    let defs = queries::find_definition(&conn, "Config").unwrap();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].kind, SymbolKind::Struct);
}
