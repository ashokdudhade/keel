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
    let count = index::index_repository(root, &mut conn).unwrap();
    assert_eq!(count, 1, "only src/lib.rs should be indexed");

    let defs = queries::find_definition(&conn, "AuthService").unwrap();
    assert_eq!(defs.len(), 1);
    assert_eq!(defs[0].kind, SymbolKind::Struct);

    let refs = queries::find_references(&conn, "create_order").unwrap();
    assert_eq!(refs.len(), 1);
    assert_eq!(refs[0].start_line, 3);

    let ignored = queries::find_definition(&conn, "should_not_index").unwrap();
    assert!(ignored.is_empty(), "ignored.rs must be skipped");
}
