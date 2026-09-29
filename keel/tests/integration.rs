//! End-to-end integration tests for the Keel engine and `keel` CLI.

use rusqlite::Connection;
use keel::api;
use keel::db::{queries, schema};
use keel::graph::deps;
use keel::graph::impact;
use keel::graph::types::SymbolKind;
use keel::index;
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

    let sb = env!("CARGO_BIN_EXE_keel");

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
fn cli_preview_shows_source_lines_in_text_and_json() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("main.rs"), "fn create_order() {}\nfn run() { create_order(); }\n").unwrap();

    let sb = env!("CARGO_BIN_EXE_keel");

    let index_out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["index", "."])
        .output()
        .unwrap();
    assert!(index_out.status.success(), "index failed: {:?}", index_out);

    let text = std::process::Command::new(sb)
        .current_dir(root)
        .args(["definition", "create_order", "--preview"])
        .output()
        .unwrap();
    assert!(text.status.success());
    let stdout = String::from_utf8(text.stdout).unwrap();
    let rows: Vec<&str> = stdout.lines().collect();
    assert_eq!(rows.len(), 2, "got: {stdout}");
    assert!(rows[1].starts_with("    fn create_order"), "got: {stdout}");

    let refs = std::process::Command::new(sb)
        .current_dir(root)
        .args(["references", "create_order", "--preview"])
        .output()
        .unwrap();
    assert!(refs.status.success());
    let ref_out = String::from_utf8(refs.stdout).unwrap();
    assert!(
        ref_out.contains("\n    fn run()"),
        "reference preview missing in: {ref_out}"
    );

    let js = std::process::Command::new(sb)
        .current_dir(root)
        .args(["--json", "definition", "create_order", "--preview"])
        .output()
        .unwrap();
    assert!(js.status.success());
    let payload: serde_json::Value = serde_json::from_slice(&js.stdout).unwrap();
    assert_eq!(
        payload["results"][0]["preview"].as_str(),
        Some("fn create_order() {}"),
        "got: {payload}"
    );

    // Without --preview the field stays out (additive JSON).
    let plain = std::process::Command::new(sb)
        .current_dir(root)
        .args(["--json", "definition", "create_order"])
        .output()
        .unwrap();
    assert!(plain.status.success());
    let payload: serde_json::Value = serde_json::from_slice(&plain.stdout).unwrap();
    assert!(
        payload["results"][0].get("preview").is_none(),
        "got: {payload}"
    );
}

#[test]
fn cli_implementations_preview_shows_source_lines() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("main.rs"),
        "trait Greeter {}\nstruct Bot;\nimpl Greeter for Bot {}\n",
    )
    .unwrap();

    let sb = env!("CARGO_BIN_EXE_keel");

    let index_out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["index", "."])
        .output()
        .unwrap();
    assert!(index_out.status.success(), "index failed: {:?}", index_out);

    let text = std::process::Command::new(sb)
        .current_dir(root)
        .args(["implementations", "Greeter", "--preview"])
        .output()
        .unwrap();
    assert!(text.status.success());
    let stdout = String::from_utf8(text.stdout).unwrap();
    assert!(
        stdout.contains("\n    impl Greeter for Bot {}"),
        "impl preview missing in: {stdout}"
    );

    let js = std::process::Command::new(sb)
        .current_dir(root)
        .args(["--json", "implementations", "Greeter", "--preview"])
        .output()
        .unwrap();
    assert!(js.status.success());
    let payload: serde_json::Value = serde_json::from_slice(&js.stdout).unwrap();
    assert_eq!(
        payload["results"][0]["preview"].as_str(),
        Some("impl Greeter for Bot {}"),
        "got: {payload}"
    );

    // Without --preview the field stays out (additive JSON).
    let plain = std::process::Command::new(sb)
        .current_dir(root)
        .args(["--json", "implementations", "Greeter"])
        .output()
        .unwrap();
    assert!(plain.status.success());
    let payload: serde_json::Value = serde_json::from_slice(&plain.stdout).unwrap();
    assert!(
        payload["results"][0].get("preview").is_none(),
        "got: {payload}"
    );
}

#[test]
fn cli_unused_sweep_flags_uncalled_functions() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("main.rs"),
        "fn live() {}\nfn dead() {}\nfn main() { live(); }\n",
    )
    .unwrap();

    let sb = env!("CARGO_BIN_EXE_keel");

    let index_out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["index", "."])
        .output()
        .unwrap();
    assert!(index_out.status.success(), "index failed: {:?}", index_out);

    let text = std::process::Command::new(sb)
        .current_dir(root)
        .args(["unused"])
        .output()
        .unwrap();
    assert!(text.status.success());
    let stdout = String::from_utf8(text.stdout).unwrap();
    assert!(
        stdout.contains("\tfunction\tdead\n"),
        "dead missing in: {stdout}"
    );
    assert!(
        !stdout.contains("\tlive\n") && !stdout.contains("\tmain\n"),
        "live/main leaked in: {stdout}"
    );

    let js = std::process::Command::new(sb)
        .current_dir(root)
        .args(["--json", "unused", "--preview"])
        .output()
        .unwrap();
    assert!(js.status.success());
    let payload: serde_json::Value = serde_json::from_slice(&js.stdout).unwrap();
    assert_eq!(payload["confidence"].as_str(), Some("low"));
    assert_eq!(
        payload["results"][0]["preview"].as_str(),
        Some("fn dead() {}"),
        "got: {payload}"
    );
}

#[test]
fn cli_definition_module_flag_disambiguates_collisions() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), "pub fn serve() {}\n").unwrap();
    std::fs::write(root.join("src/b.rs"), "pub fn serve() {}\n").unwrap();

    let sb = env!("CARGO_BIN_EXE_keel");

    let index_out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["index", "."])
        .output()
        .unwrap();
    assert!(index_out.status.success(), "index failed: {:?}", index_out);

    let both = std::process::Command::new(sb)
        .current_dir(root)
        .args(["definition", "serve"])
        .output()
        .unwrap();
    assert!(both.status.success());
    let both_out = String::from_utf8(both.stdout).unwrap();
    assert_eq!(both_out.lines().count(), 2, "got: {both_out}");

    let one = std::process::Command::new(sb)
        .current_dir(root)
        .args(["definition", "serve", "--module", "crate::a"])
        .output()
        .unwrap();
    assert!(one.status.success());
    let one_out = String::from_utf8(one.stdout).unwrap();
    assert!(
        one_out.contains("src/a.rs") && !one_out.contains("src/b.rs"),
        "got: {one_out}"
    );
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

    let sb = env!("CARGO_BIN_EXE_keel");

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

    let sb = env!("CARGO_BIN_EXE_keel");
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

    let sb = env!("CARGO_BIN_EXE_keel");
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
fn cli_references_limit_caps_hits_with_stderr_note() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "fn target() {}\nfn u1() { target(); }\nfn u2() { target(); }\nfn u3() { target(); }\n",
    )
    .unwrap();

    let sb = env!("CARGO_BIN_EXE_keel");
    let index_out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["index", "."])
        .output()
        .unwrap();
    assert!(index_out.status.success(), "index failed: {:?}", index_out);

    let out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["references", "target", "--limit", "2"])
        .output()
        .unwrap();
    assert!(out.status.success(), "references failed: {:?}", out);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 2, "got: {stdout}");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("Showing first 2 of 3 matches"),
        "truncation must be visible in text mode, got: {stderr}"
    );
}

#[test]
fn cli_dependents_limit_caps_hits_with_stderr_note() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("a.py"), "def core():\n    pass\n").unwrap();
    for name in ["x.py", "y.py", "z.py"] {
        fs::write(root.join(name), "import a\n\na.core()\n").unwrap();
    }

    let sb = env!("CARGO_BIN_EXE_keel");
    let index_out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["index", "."])
        .output()
        .unwrap();
    assert!(index_out.status.success(), "index failed: {:?}", index_out);

    let out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["dependents", "a", "--limit", "2"])
        .output()
        .unwrap();
    assert!(out.status.success(), "dependents failed: {:?}", out);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 2, "got: {stdout}");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("Showing first 2 of 3 matches"),
        "truncation must be visible in text mode, got: {stderr}"
    );
}

#[test]
fn cli_dependencies_limit_caps_hits_with_stderr_note() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for name in ["a.py", "b.py", "c.py"] {
        fs::write(root.join(name), "def f():\n    pass\n").unwrap();
    }
    fs::write(root.join("main.py"), "import a\nimport b\nimport c\n").unwrap();

    let sb = env!("CARGO_BIN_EXE_keel");
    let index_out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["index", "."])
        .output()
        .unwrap();
    assert!(index_out.status.success(), "index failed: {:?}", index_out);

    let out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["dependencies", "main.py", "--limit", "2"])
        .output()
        .unwrap();
    assert!(out.status.success(), "dependencies failed: {:?}", out);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 2, "got: {stdout}");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("Showing first 2 of 3 matches"),
        "truncation must be visible in text mode, got: {stderr}"
    );
}

#[test]
fn cli_implementations_limit_caps_hits_with_stderr_note() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "trait T {}\nstruct A;\nstruct B;\nstruct C;\nimpl T for A {}\nimpl T for B {}\nimpl T for C {}\n",
    )
    .unwrap();

    let sb = env!("CARGO_BIN_EXE_keel");
    let index_out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["index", "."])
        .output()
        .unwrap();
    assert!(index_out.status.success(), "index failed: {:?}", index_out);

    let out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["implementations", "T", "--limit", "2"])
        .output()
        .unwrap();
    assert!(out.status.success(), "implementations failed: {:?}", out);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 2, "got: {stdout}");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("Showing first 2 of 3 matches"),
        "truncation must be visible in text mode, got: {stderr}"
    );
}

#[test]
fn cli_definition_limit_caps_hits_with_stderr_note() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for name in ["a.py", "b.py", "c.py"] {
        fs::write(root.join(name), "def dup():\n    pass\n").unwrap();
    }

    let sb = env!("CARGO_BIN_EXE_keel");
    let index_out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["index", "."])
        .output()
        .unwrap();
    assert!(index_out.status.success(), "index failed: {:?}", index_out);

    let out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["definition", "dup", "--limit", "2"])
        .output()
        .unwrap();
    assert!(out.status.success(), "definition failed: {:?}", out);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 2, "got: {stdout}");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("Showing first 2 of 3 matches"),
        "truncation must be visible in text mode, got: {stderr}"
    );
}

#[test]
fn cli_outline_limit_caps_hits_with_stderr_note() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(
        root.join("lib.py"),
        "def a():\n    pass\ndef b():\n    pass\ndef c():\n    pass\n",
    )
    .unwrap();

    let sb = env!("CARGO_BIN_EXE_keel");
    let index_out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["index", "."])
        .output()
        .unwrap();
    assert!(index_out.status.success(), "index failed: {:?}", index_out);

    let out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["outline", "lib.py", "--limit", "2"])
        .output()
        .unwrap();
    assert!(out.status.success(), "outline failed: {:?}", out);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 2, "got: {stdout}");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("Showing first 2 of 3 matches"),
        "truncation must be visible in text mode, got: {stderr}"
    );
}

#[test]
fn cli_empty_name_rejected_with_usage_error() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("a.py"), "def f():\n    pass\n").unwrap();

    let sb = env!("CARGO_BIN_EXE_keel");
    for args in [&["definition", ""], &["search", ""]] {
        let out = std::process::Command::new(sb)
            .current_dir(root)
            .args(*args)
            .output()
            .unwrap();
        assert!(
            !out.status.success(),
            "{args:?} must fail, got: {out:?}"
        );
        let stderr = String::from_utf8(out.stderr).unwrap();
        assert!(
            stderr.contains("must not be empty"),
            "{args:?} must name the problem, got: {stderr}"
        );
    }
}

#[test]
fn cli_index_and_init_report_syntax_errors() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::write(root.join("a.py"), "def f():\n    pass\n").unwrap();
    fs::write(root.join("bad.py"), "def broken(:\n  ???\n").unwrap();

    let sb = env!("CARGO_BIN_EXE_keel");
    let out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["index", "."])
        .output()
        .unwrap();
    assert!(out.status.success(), "index failed: {:?}", out);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("syntax errors 1"),
        "index must report the bucket, got: {stdout}"
    );
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("index warning: bad.py"),
        "index must name the file, got: {stderr}"
    );

    std::fs::remove_dir_all(root.join(".keel")).unwrap();
    // Isolate the daemon probe: otherwise a live daemon (or any /health on
    // the default port) would flip `init` into register-with-daemon mode.
    let daemon_port = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let out = std::process::Command::new(sb)
        .current_dir(root)
        .env("KEEL_HOME", dir.path().join("keelhome"))
        .env("KEEL_DAEMON_PORT", daemon_port.to_string())
        .args(["init", "."])
        .output()
        .unwrap();
    assert!(out.status.success(), "init failed: {:?}", out);
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("syntax errors 1"),
        "init must share the stats format, got: {stdout}"
    );
}

#[test]
fn cli_doctor_reports_mcp_loopback_ok() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();

    let sb = env!("CARGO_BIN_EXE_keel");
    let out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["doctor", "."])
        .output()
        .unwrap();
    assert!(out.status.success(), "doctor failed: {:?}", out);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let mcp = stdout
        .lines()
        .find(|l| l.starts_with("mcp:"))
        .expect("doctor must include an mcp line");
    assert!(
        mcp.contains("\tok\t"),
        "mcp loopback should pass, got: {mcp}"
    );
}

#[test]
fn cli_doctor_missing_index_advice_matches_daemon_state() {
    // Fresh dir, no index: advice must be runnable — `keel init` works
    // daemon-less, `keel start` needs the daemon. Robust to a daemon
    // happening to run on the test machine.
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();

    let sb = env!("CARGO_BIN_EXE_keel");
    let out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["doctor", "."])
        .output()
        .unwrap();
    assert!(out.status.success(), "doctor failed: {:?}", out);
    let stdout = String::from_utf8(out.stdout).unwrap();
    let daemon_up = stdout
        .lines()
        .find(|l| l.starts_with("daemon:"))
        .expect("doctor must include a daemon line")
        .contains("\tok\t");
    let index = stdout
        .lines()
        .find(|l| l.starts_with("index:"))
        .expect("doctor must include an index line");
    let expected = if daemon_up {
        "run: keel start ."
    } else {
        "run: keel init ."
    };
    assert!(
        index.contains(expected),
        "daemon_up={daemon_up}, index line: {index}"
    );
    let project = stdout
        .lines()
        .find(|l| l.starts_with("project:"))
        .expect("doctor must include a project line");
    if !daemon_up {
        assert!(
            project.contains("keel daemon"),
            "project advice must name the daemon first, got: {project}"
        );
    }
}

#[test]
fn cli_doctor_json_reports_checks() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();

    let sb = env!("CARGO_BIN_EXE_keel");
    let out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["doctor", ".", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "doctor --json failed: {:?}", out);
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();
    let checks = v["checks"].as_array().unwrap();
    let get = |name: &str| checks.iter().find(|c| c["name"] == name).unwrap();
    assert_eq!(get("version")["ok"], true);
    assert_eq!(
        get("mcp")["ok"],
        true,
        "mcp loopback should pass: {}",
        get("mcp")
    );
}

#[test]
fn cli_index_json_reports_stats() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();

    let sb = env!("CARGO_BIN_EXE_keel");
    let out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["index", ".", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "index --json failed: {:?}", out);
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();
    assert_eq!(v["indexed"], 1);
    assert_eq!(v["skipped"], 0);
    assert_eq!(v["errors"], 0);
}

#[test]
fn cli_status_json_reports_daemon_and_insights() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("main.rs"), "fn main() {}\n").unwrap();

    let sb = env!("CARGO_BIN_EXE_keel");
    let out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["status", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "status --json failed: {:?}", out);
    let v: serde_json::Value =
        serde_json::from_str(&String::from_utf8(out.stdout).unwrap()).unwrap();
    assert!(v["daemon"]["daemon"].is_string());
    // Fresh temp project: no insights server was ever started here.
    assert_eq!(v["insights"]["state"], "stopped");
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
        let _ = api::serve(&format!("127.0.0.1:{port}"), &serve_db, false);
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

#[test]
fn json_api_preview_param_adds_source_lines() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub struct AuthService;\nfn create_order() {}\nfn caller() { create_order(); }\ntrait Greeter {}\nstruct Bot;\nimpl Greeter for Bot {}\n",
    )
    .unwrap();

    let db_path: PathBuf = root.join(".keel/index.db");
    fs::create_dir_all(root.join(".keel")).unwrap();
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
        let _ = api::serve(&format!("127.0.0.1:{port}"), &serve_db, false);
    });

    wait_for_port(port);

    let body = http_get(port, "/symbol/create_order?preview=1");
    let symbol: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(
        symbol["definition"][0]["preview"].as_str(),
        Some("fn create_order() {}"),
        "got: {body}"
    );
    assert_eq!(
        symbol["references"][0]["preview"].as_str(),
        Some("fn caller() { create_order(); }"),
        "got: {body}"
    );

    // Implementation rows preview too (they are file:line hits).
    let impls = http_get(port, "/symbol/Greeter?preview=1");
    let greeter: serde_json::Value = serde_json::from_str(&impls).unwrap();
    assert_eq!(
        greeter["implementations"][0]["preview"].as_str(),
        Some("impl Greeter for Bot {}"),
        "got: {impls}"
    );

    // Without the param the field stays out.
    let plain = http_get(port, "/symbol/create_order");
    let symbol: serde_json::Value = serde_json::from_str(&plain).unwrap();
    assert!(
        symbol["definition"][0].get("preview").is_none(),
        "got: {plain}"
    );
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
fn indexes_multi_language_monorepo_in_single_pass() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("rust/src")).unwrap();
    fs::create_dir_all(root.join("ts/src")).unwrap();
    fs::create_dir_all(root.join("go/pkg")).unwrap();
    fs::create_dir_all(root.join("js/src")).unwrap();
    fs::create_dir_all(root.join("py/pkg")).unwrap();
    fs::write(
        root.join("rust/src/lib.rs"),
        "pub struct RustService;\nfn rust_helper() {}\n",
    )
    .unwrap();
    fs::write(
        root.join("ts/src/service.ts"),
        "export class TsService {\n  run(): void {}\n}\nexport function tsHelper(): void {}\n",
    )
    .unwrap();
    fs::write(
        root.join("go/pkg/service.go"),
        "package pkg\n\ntype GoService struct{}\n\nfunc GoHelper() {}\n",
    )
    .unwrap();
    fs::write(
        root.join("js/src/service.js"),
        "export class JsService {\n  run() {}\n}\nexport function jsHelper() {}\n",
    )
    .unwrap();
    fs::write(
        root.join("js/src/Widget.jsx"),
        "export function JsxWidget() { return <div/>; }\n",
    )
    .unwrap();
    fs::write(
        root.join("py/pkg/service.py"),
        "class PyService:\n    def run(self):\n        pass\n\ndef py_helper():\n    pass\n",
    )
    .unwrap();
    fs::write(
        root.join("py/pkg/types.pyi"),
        "class PyStub: ...\n",
    )
    .unwrap();

    let mut conn = Connection::open_in_memory().unwrap();
    let stats = index::index_repository(root, &mut conn).unwrap();
    assert_eq!(stats.indexed, 7, "expected one file per language extension");

    for (name, kind) in [
        ("RustService", SymbolKind::Struct),
        ("TsService", SymbolKind::Struct),
        ("GoService", SymbolKind::Struct),
        ("JsService", SymbolKind::Struct),
        ("JsxWidget", SymbolKind::Function),
        ("PyService", SymbolKind::Struct),
        ("PyStub", SymbolKind::Struct),
    ] {
        let defs = queries::find_definition(&conn, name).unwrap();
        assert_eq!(defs.len(), 1, "{name}");
        assert_eq!(defs[0].kind, kind, "{name}");
    }
    assert_eq!(
        queries::find_definition(&conn, "GoService").unwrap()[0].module_path,
        "pkg"
    );
    assert_eq!(
        queries::find_definition(&conn, "PyService").unwrap()[0].module_path,
        "py.pkg.service"
    );
}

#[test]
fn indexes_javascript_fixture_and_finds_symbol() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/auth.js"),
        "export class AuthService {\n  login() {}\n}\nexport function createOrder() {}\n",
    )
    .unwrap();

    let mut conn = Connection::open_in_memory().unwrap();
    let stats = index::index_repository(root, &mut conn).unwrap();
    assert_eq!(stats.indexed, 1);

    let defs = queries::find_definition(&conn, "AuthService").unwrap();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].kind, SymbolKind::Struct);
    assert_eq!(defs[0].module_path, "src/auth");
    assert_eq!(defs[0].file.as_os_str(), "src/auth.js");

    let fns = queries::find_definition(&conn, "createOrder").unwrap();
    assert_eq!(fns.len(), 1);
    assert_eq!(fns[0].kind, SymbolKind::Function);
}

#[test]
fn indexes_python_fixture_and_finds_symbol() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("auth")).unwrap();
    fs::write(
        root.join("auth/service.py"),
        "class User:\n    pass\n\ndef create_order():\n    pass\n",
    )
    .unwrap();

    let mut conn = Connection::open_in_memory().unwrap();
    let stats = index::index_repository(root, &mut conn).unwrap();
    assert_eq!(stats.indexed, 1);

    let defs = queries::find_definition(&conn, "create_order").unwrap();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].kind, SymbolKind::Function);
    assert_eq!(defs[0].module_path, "auth.service");

    let types = queries::find_definition(&conn, "User").unwrap();
    assert_eq!(types.len(), 1);
    assert_eq!(types[0].kind, SymbolKind::Struct);
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
    assert_eq!(defs[0].module_path, "src/auth");
    assert_eq!(defs[0].file.as_os_str(), "src/auth.ts");

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

#[test]
fn cli_definition_trims_padded_name() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("svc.py"), "def save():\n    pass\n").unwrap();

    let sb = env!("CARGO_BIN_EXE_keel");
    let out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["definition", "  save "])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("svc.py:1:5"),
        "padded name must hit, got: {stdout}"
    );
}

#[test]
#[cfg(unix)]
fn cli_definition_queries_read_only_db_without_auto_index() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("svc.py"), "def save():\n    pass\n").unwrap();

    let sb = env!("CARGO_BIN_EXE_keel");
    let out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["index", "."])
        .output()
        .unwrap();
    assert!(out.status.success());

    let db = root.join(".keel/index.db");
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o444)).unwrap();
    let out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["definition", "save", "--no-auto-index"])
        .output()
        .unwrap();
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(out.status.success(), "stderr: {}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("svc.py:1:5"), "got: {stdout}");
}

#[test]
fn cli_queries_resolve_project_root_from_subdir() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir_all(root.join("sub/deep")).unwrap();
    std::fs::write(root.join("r.py"), "def rootfn():\n    pass\n").unwrap();

    let sb = env!("CARGO_BIN_EXE_keel");
    let out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["index", "."])
        .output()
        .unwrap();
    assert!(out.status.success());

    // Queries from a nested subdir read the ancestor index (and must not
    // strand a stray `.keel/` in the subdir).
    let out = std::process::Command::new(sb)
        .current_dir(root.join("sub/deep"))
        .args(["definition", "rootfn"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("rootfn"), "got: {stdout}");
    assert!(
        !root.join("sub/deep/.keel").exists(),
        "subdir query must not create a stray index"
    );
}

#[test]
fn cli_index_caps_per_file_syntax_warnings() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    for i in 0..12 {
        std::fs::write(root.join(format!("broken{i}.py")), "def broken(:\n  pass\n").unwrap();
    }

    let sb = env!("CARGO_BIN_EXE_keel");
    let out = std::process::Command::new(sb)
        .current_dir(root)
        .args(["index", "."])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stderr = String::from_utf8(out.stderr).unwrap();
    let warnings = stderr
        .lines()
        .filter(|l| l.contains("symbols may be incomplete"))
        .count();
    assert_eq!(warnings, 10, "stderr was:\n{stderr}");
    assert!(
        stderr.contains("... and 2 more file(s) with syntax errors"),
        "missing summary in:\n{stderr}"
    );
}
