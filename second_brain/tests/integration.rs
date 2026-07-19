//! End-to-end integration tests for the SecondBrain engine and `sb` CLI.

use rusqlite::Connection;
use second_brain::api;
use second_brain::db::{queries, schema};
use second_brain::graph::deps;
use second_brain::graph::impact;
use second_brain::graph::types::SymbolKind;
use second_brain::index;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

#[test]
fn indexes_and_queries_a_fixture_repo() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub struct AuthService;\nfn create_order() {}\nfn caller() { create_order(); }\n",
    )
    .unwrap();
    // A file that should be ignored via .gitignore semantics.
    fs::write(root.join(".gitignore"), "ignored.rs\n").unwrap();
    fs::write(root.join("ignored.rs"), "fn should_not_index() {}\n").unwrap();

    let mut conn = Connection::open_in_memory().unwrap();
    let stats = index::index_repository(root, &mut conn).unwrap();
    assert_eq!(stats.indexed, 1, "only src/lib.rs should be indexed");
    assert_eq!(stats.skipped, 0);
    assert_eq!(stats.removed, 0);

    let defs = queries::find_definition(&conn, "AuthService").unwrap();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].kind, SymbolKind::Struct);

    let refs = queries::find_references(&conn, "create_order").unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].start_line, 3);

    let ignored = queries::find_definition(&conn, "should_not_index").unwrap();
    assert!(ignored.is_empty(), "ignored.rs must be skipped");
}

#[test]
fn reindexing_same_repo_does_not_duplicate_rows() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub struct AuthService;\nfn create_order() {}\nfn caller() { create_order(); }\n",
    )
    .unwrap();

    let mut conn = Connection::open_in_memory().unwrap();

    let first = index::index_repository(root, &mut conn).unwrap();
    assert_eq!(first.indexed, 1);
    let defs_first = queries::find_definition(&conn, "AuthService").unwrap();
    let refs_first = queries::find_references(&conn, "create_order").unwrap();
    assert_eq!(defs_first.len(), 1);
    assert_eq!(refs_first.len(), 1);

    // Re-index the identical repo into the SAME connection.
    let second = index::index_repository(root, &mut conn).unwrap();
    assert_eq!(second.indexed, 0);
    assert_eq!(second.skipped, 1);
    assert_eq!(second.removed, 0);

    let defs_second = queries::find_definition(&conn, "AuthService").unwrap();
    let refs_second = queries::find_references(&conn, "create_order").unwrap();
    assert_eq!(defs_second.len(), 1, "re-index must not duplicate symbols");
    assert_eq!(
        refs_second.len(),
        refs_first.len(),
        "re-index must not duplicate references"
    );
}

#[test]
fn incremental_second_pass_skips_unchanged_files() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "fn alpha() {}\n").unwrap();
    fs::write(root.join("src/b.rs"), "fn beta() {}\n").unwrap();

    let mut conn = Connection::open_in_memory().unwrap();
    let first = index::index_repository(root, &mut conn).unwrap();
    assert_eq!(first.indexed, 2);
    assert_eq!(first.skipped, 0);

    let second = index::index_repository(root, &mut conn).unwrap();
    assert_eq!(second.indexed, 0);
    assert_eq!(second.skipped, 2);
    assert_eq!(second.removed, 0);
}

#[test]
fn incremental_reindexes_only_modified_file() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/a.rs"), "fn alpha() {}\n").unwrap();
    fs::write(root.join("src/b.rs"), "fn beta() {}\n").unwrap();

    let mut conn = Connection::open_in_memory().unwrap();
    let first = index::index_repository(root, &mut conn).unwrap();
    assert_eq!(first.indexed, 2);

    fs::write(root.join("src/a.rs"), "fn alpha() {}\nfn gamma() {}\n").unwrap();

    let second = index::index_repository(root, &mut conn).unwrap();
    assert_eq!(second.indexed, 1);
    assert_eq!(second.skipped, 1);
    assert_eq!(second.removed, 0);

    let gamma = queries::find_definition(&conn, "gamma").unwrap();
    assert_eq!(gamma.len(), 1);
    assert!(gamma[0].file.ends_with("src/a.rs"));

    let beta = queries::find_definition(&conn, "beta").unwrap();
    assert_eq!(beta.len(), 1);
}

#[test]
fn incremental_removes_deleted_file_rows() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/keep.rs"), "fn keep() {}\n").unwrap();
    fs::write(root.join("src/gone.rs"), "fn gone() {}\n").unwrap();

    let mut conn = Connection::open_in_memory().unwrap();
    let first = index::index_repository(root, &mut conn).unwrap();
    assert_eq!(first.indexed, 2);
    assert_eq!(queries::find_definition(&conn, "gone").unwrap().len(), 1);

    fs::remove_file(root.join("src/gone.rs")).unwrap();

    let second = index::index_repository(root, &mut conn).unwrap();
    assert_eq!(second.indexed, 0);
    assert_eq!(second.skipped, 1);
    assert_eq!(second.removed, 1);

    assert!(queries::find_definition(&conn, "gone").unwrap().is_empty());
    assert_eq!(queries::find_definition(&conn, "keep").unwrap().len(), 1);
}

#[test]
fn cli_binary_indexes_and_queries() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("main.rs"), "fn create_order() {}\nfn run() { create_order(); }\n").unwrap();

    let sb = env!("CARGO_BIN_EXE_sb");

    let index_out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["index", "."])
        .output()
        .unwrap();
    assert!(index_out.status.success(), "index failed: {:?}", index_out);

    let def_out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["definition", "create_order"])
        .output()
        .unwrap();
    assert!(def_out.status.success());
    let stdout = String::from_utf8(def_out.stdout).unwrap();
    assert!(stdout.contains("create_order"), "got: {stdout}");
    assert!(stdout.contains(":1:"), "expected line 1 in: {stdout}");
}

#[test]
fn find_implementations_returns_trait_impls_excludes_inherent() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        r#"
pub trait Storage {}

pub struct A;
pub struct B;

impl Storage for A {}

impl Storage for B {}

impl A {}
"#,
    )
    .unwrap();

    let mut conn = Connection::open_in_memory().unwrap();
    index::index_repository(root, &mut conn).unwrap();

    let impls = queries::find_implementations(&conn, "Storage").unwrap();
    assert_eq!(impls.len(), 2, "expected two trait impls, got {impls:?}");
    assert_eq!(impls[0].type_name, "A");
    assert_eq!(impls[0].trait_name.as_deref(), Some("Storage"));
    assert_eq!(impls[1].type_name, "B");
    assert_eq!(impls[1].trait_name.as_deref(), Some("Storage"));
    assert!(
        impls[0].start_line <= impls[1].start_line,
        "expected path/line/col order: {impls:?}"
    );
}

#[test]
fn cli_implementations_prints_trait_impls() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub trait Storage {}\npub struct A;\npub struct B;\nimpl Storage for A {}\nimpl Storage for B {}\nimpl A {}\n",
    )
    .unwrap();

    let sb = env!("CARGO_BIN_EXE_sb");

    let index_out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["index", "."])
        .output()
        .unwrap();
    assert!(index_out.status.success(), "index failed: {:?}", index_out);

    let impl_out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["implementations", "Storage"])
        .output()
        .unwrap();
    assert!(impl_out.status.success(), "implementations failed: {:?}", impl_out);
    let stdout = String::from_utf8(impl_out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "got: {stdout}");
    assert!(lines[0].ends_with("\tA"), "got: {}", lines[0]);
    assert!(lines[1].ends_with("\tB"), "got: {}", lines[1]);
    assert!(lines[0].contains(':'), "expected path:line:col in: {}", lines[0]);
}

#[test]
fn find_dependencies_from_indexed_fixture() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    // `a`/`b` share a file (imports are file-scoped); `leaf` is a separate
    // file with no imports so the empty-deps case is meaningful under indexing.
    fs::write(
        root.join("src/lib.rs"),
        r#"
mod leaf;

mod b {
    pub fn f() {}
}

mod a {
    use crate::b;
    pub fn g() {
        b::f();
    }
}
"#,
    )
    .unwrap();
    fs::write(root.join("src/leaf.rs"), "pub fn alone() {}\n").unwrap();

    let mut conn = Connection::open_in_memory().unwrap();
    index::index_repository(root, &mut conn).unwrap();

    let deps = deps::find_dependencies(&conn, "crate::a").unwrap();
    let paths: Vec<&str> = deps.iter().map(|d| d.module_path.as_str()).collect();
    assert!(
        paths.contains(&"crate::b"),
        "expected crate::b dependency, got {paths:?}"
    );

    let leaf = deps::find_dependencies(&conn, "alone").unwrap();
    assert!(leaf.is_empty(), "leaf must have no deps: {leaf:?}");
}

#[test]
fn cli_dependencies_prints_imported_modules() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "mod b { pub fn f() {} }\nmod a { use crate::b; pub fn g() { b::f(); } }\n",
    )
    .unwrap();

    let sb = env!("CARGO_BIN_EXE_sb");
    let index_out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["index", "."])
        .output()
        .unwrap();
    assert!(index_out.status.success(), "index failed: {:?}", index_out);

    let dep_out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["dependencies", "crate::a"])
        .output()
        .unwrap();
    assert!(dep_out.status.success(), "dependencies failed: {:?}", dep_out);
    let stdout = String::from_utf8(dep_out.stdout).unwrap();
    assert!(
        stdout.lines().any(|l| l.starts_with("crate::b")),
        "expected crate::b in: {stdout}"
    );
}

#[test]
fn find_impact_from_indexed_call_chain() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "fn a() {}\nfn b() { a(); }\nfn c() { b(); }\nfn lonely() {}\n",
    )
    .unwrap();

    let mut conn = Connection::open_in_memory().unwrap();
    index::index_repository(root, &mut conn).unwrap();

    let impacted = impact::find_impact(&conn, "a").unwrap();
    let names: Vec<&str> = impacted.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["b", "c"], "got {names:?}");

    let none = impact::find_impact(&conn, "lonely").unwrap();
    assert!(none.is_empty(), "lonely must have empty impact: {none:?}");
}

#[test]
fn cli_impact_prints_transitive_callers() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "fn a() {}\nfn b() { a(); }\nfn c() { b(); }\n",
    )
    .unwrap();

    let sb = env!("CARGO_BIN_EXE_sb");
    let index_out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["index", "."])
        .output()
        .unwrap();
    assert!(index_out.status.success(), "index failed: {:?}", index_out);

    let impact_out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["impact", "a"])
        .output()
        .unwrap();
    assert!(impact_out.status.success(), "impact failed: {:?}", impact_out);
    let stdout = String::from_utf8(impact_out.stdout).unwrap();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "got: {stdout}");
    assert!(lines[0].ends_with("\tb"), "got: {}", lines[0]);
    assert!(lines[1].ends_with("\tc"), "got: {}", lines[1]);
}

#[test]
fn json_api_serves_symbol_and_health() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub struct AuthService;\nfn create_order() {}\nfn caller() { create_order(); }\n",
    )
    .unwrap();

    let db_path: PathBuf = root.join("index.db");
    {
        let mut conn = Connection::open(&db_path).unwrap();
        schema::initialize(&conn).unwrap();
        index::index_repository(root, &mut conn).unwrap();
    }

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    drop(listener);

    let serve_db = db_path.clone();
    thread::spawn(move || {
        let _ = api::serve(&format!("127.0.0.1:{port}"), &serve_db);
    });

    wait_for_port(port);

    let health_body = http_get(port, "/health");
    let health: serde_json::Value = serde_json::from_str(&health_body).unwrap();
    assert_eq!(health["status"], "ok");

    let symbol_body = http_get(port, "/symbol/AuthService");
    let symbol: serde_json::Value = serde_json::from_str(&symbol_body).unwrap();

    assert!(symbol["definition"].is_array());
    assert!(symbol["references"].is_array());
    assert!(symbol["implementations"].is_array());
    assert!(symbol["dependencies"].is_array());
    assert!(symbol["callers"].is_array());

    let defs = symbol["definition"].as_array().unwrap();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0]["name"], "AuthService");
    assert_eq!(defs[0]["kind"], "struct");
    let file = defs[0]["file"].as_str().expect("file must be a string path");
    assert!(
        file.ends_with("src/lib.rs") || file.ends_with("src\\lib.rs"),
        "unexpected file path: {file}"
    );

    // Determinism: arrays stay ordered across repeated GETs.
    let again = http_get(port, "/symbol/AuthService");
    assert_eq!(symbol_body, again);
}

fn wait_for_port(port: u16) {
    for _ in 0..50 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    panic!("server did not start on port {port}");
}

fn http_get(port: u16, path: &str) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).unwrap();
    let text = String::from_utf8_lossy(&buf);
    let split = text
        .find("\r\n\r\n")
        .expect("HTTP response missing header/body separator");
    text[split + 4..].to_string()
}

#[test]
fn indexes_typescript_fixture_and_finds_symbol() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/auth.ts"),
        "export class AuthService {\n  login(): void {}\n}\nexport function createOrder(): void {}\n",
    )
    .unwrap();

    let mut conn = Connection::open_in_memory().unwrap();
    let stats = index::index_repository(root, &mut conn).unwrap();
    assert_eq!(stats.indexed, 1);

    let defs = queries::find_definition(&conn, "AuthService").unwrap();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].kind, SymbolKind::Struct);

    let fns = queries::find_definition(&conn, "createOrder").unwrap();
    assert_eq!(fns.len(), 1);
    assert_eq!(fns[0].kind, SymbolKind::Function);
}

#[test]
fn indexes_go_fixture_and_finds_symbol() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("auth")).unwrap();
    fs::write(
        root.join("auth/service.go"),
        "package auth\n\ntype User struct{}\n\nfunc CreateOrder() {}\n",
    )
    .unwrap();

    let mut conn = Connection::open_in_memory().unwrap();
    let stats = index::index_repository(root, &mut conn).unwrap();
    assert_eq!(stats.indexed, 1);

    let defs = queries::find_definition(&conn, "CreateOrder").unwrap();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].kind, SymbolKind::Function);
    assert_eq!(defs[0].module_path, "auth");

    let types = queries::find_definition(&conn, "User").unwrap();
    assert_eq!(types.len(), 1);
    assert_eq!(types[0].kind, SymbolKind::Struct);
}
