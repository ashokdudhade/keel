//! End-to-end integration tests for the SecondBrain engine and `sb` CLI.

use rusqlite::Connection;
use second_brain::db::queries;
use second_brain::graph::types::SymbolKind;
use second_brain::index;
use std::fs;

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
