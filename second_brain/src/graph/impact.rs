//! Transitive impact analysis: who (directly or indirectly) references a name.

use crate::db::queries;
use crate::error::Result;
use crate::graph::types::Symbol;
use rusqlite::Connection;
use std::collections::{BTreeSet, HashSet};

/// Return the transitive set of symbols that reference `name`.
///
/// Expansion: references to the current name → their `container` symbols →
/// references to those containers, until fixpoint. Uses a sorted worklist and
/// a visited set so cycles terminate. Results are ordered by
/// `(name, path, line, col)` and de-duplicated by symbol identity
/// `(name, module_path, path, line, col)`.
pub fn find_impact(conn: &Connection, name: &str) -> Result<Vec<Symbol>> {
    let mut visited: HashSet<String> = HashSet::new();
    visited.insert(name.to_string());

    let mut worklist: BTreeSet<String> = BTreeSet::new();
    worklist.insert(name.to_string());

    let mut impact: Vec<Symbol> = Vec::new();
    let mut seen_symbols: HashSet<(String, String, String, u32, u32)> = HashSet::new();

    while let Some(current) = pop_front(&mut worklist) {
        for reference in queries::find_references(conn, &current)? {
            let container = container_name(&reference.container);
            if container.is_empty() || visited.contains(container) {
                continue;
            }
            visited.insert(container.to_string());
            worklist.insert(container.to_string());

            for sym in queries::find_definition(conn, container)? {
                let key = (
                    sym.name.clone(),
                    sym.module_path.clone(),
                    sym.file.to_string_lossy().into_owned(),
                    sym.start_line,
                    sym.start_col,
                );
                if seen_symbols.insert(key) {
                    impact.push(sym);
                }
            }
        }
    }

    impact.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.start_line.cmp(&b.start_line))
            .then_with(|| a.start_col.cmp(&b.start_col))
    });
    Ok(impact)
}

fn pop_front(worklist: &mut BTreeSet<String>) -> Option<String> {
    let next = worklist.iter().next()?.clone();
    worklist.remove(&next);
    Some(next)
}

/// Bare identifier for a container (handles qualified `crate::mod::fn` values).
fn container_name(container: &str) -> &str {
    container.rsplit("::").next().unwrap_or(container)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{queries, schema};
    use crate::graph::types::{FileNode, Reference, ReferenceKind, SymbolKind};
    use std::path::PathBuf;

    fn setup() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        schema::initialize(&conn).unwrap();
        conn
    }

    /// Chain: `a` ← `b` ← `c` (b calls a, c calls b).
    fn fixture_call_chain(conn: &Connection) {
        let f = queries::insert_file(
            conn,
            &FileNode {
                path: PathBuf::from("src/lib.rs"),
                content_hash: "h".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            conn,
            f,
            &[
                Symbol {
                    name: "a".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: 1,
                    start_col: 1,
                    module_path: "crate".into(),
                },
                Symbol {
                    name: "b".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: 2,
                    start_col: 1,
                    module_path: "crate".into(),
                },
                Symbol {
                    name: "c".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: 3,
                    start_col: 1,
                    module_path: "crate".into(),
                },
                Symbol {
                    name: "lonely".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: 4,
                    start_col: 1,
                    module_path: "crate".into(),
                },
            ],
        )
        .unwrap();
        queries::insert_references(
            conn,
            f,
            &[
                Reference {
                    name: "a".into(),
                    file: PathBuf::new(),
                    start_line: 2,
                    start_col: 10,
                    kind: ReferenceKind::Call,
                    container: "b".into(),
                },
                Reference {
                    name: "b".into(),
                    file: PathBuf::new(),
                    start_line: 3,
                    start_col: 10,
                    kind: ReferenceKind::Call,
                    container: "c".into(),
                },
            ],
        )
        .unwrap();
    }

    /// Cycle: `x` → `y` → `z` → `x` (and a mutual edge `x` ↔ `y`).
    fn fixture_cycle(conn: &Connection) {
        let f = queries::insert_file(
            conn,
            &FileNode {
                path: PathBuf::from("src/cycle.rs"),
                content_hash: "hc".into(),
            },
        )
        .unwrap();
        queries::insert_symbols(
            conn,
            f,
            &[
                Symbol {
                    name: "x".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: 1,
                    start_col: 1,
                    module_path: "crate".into(),
                },
                Symbol {
                    name: "y".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: 2,
                    start_col: 1,
                    module_path: "crate".into(),
                },
                Symbol {
                    name: "z".into(),
                    kind: SymbolKind::Function,
                    file: PathBuf::new(),
                    start_line: 3,
                    start_col: 1,
                    module_path: "crate".into(),
                },
            ],
        )
        .unwrap();
        queries::insert_references(
            conn,
            f,
            &[
                Reference {
                    name: "x".into(),
                    file: PathBuf::new(),
                    start_line: 2,
                    start_col: 10,
                    kind: ReferenceKind::Call,
                    container: "y".into(),
                },
                Reference {
                    name: "y".into(),
                    file: PathBuf::new(),
                    start_line: 3,
                    start_col: 10,
                    kind: ReferenceKind::Call,
                    container: "z".into(),
                },
                Reference {
                    name: "z".into(),
                    file: PathBuf::new(),
                    start_line: 1,
                    start_col: 10,
                    kind: ReferenceKind::Call,
                    container: "x".into(),
                },
                Reference {
                    name: "y".into(),
                    file: PathBuf::new(),
                    start_line: 1,
                    start_col: 20,
                    kind: ReferenceKind::Call,
                    container: "x".into(),
                },
            ],
        )
        .unwrap();
    }

    #[test]
    fn find_impact_returns_transitive_callers() {
        let conn = setup();
        fixture_call_chain(&conn);

        let impact = find_impact(&conn, "a").unwrap();
        let names: Vec<&str> = impact.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["b", "c"]);
    }

    #[test]
    fn find_impact_no_callers_returns_empty() {
        let conn = setup();
        fixture_call_chain(&conn);

        let impact = find_impact(&conn, "lonely").unwrap();
        assert!(impact.is_empty(), "expected empty, got {impact:?}");
    }

    #[test]
    fn find_impact_terminates_on_cycle() {
        let conn = setup();
        fixture_cycle(&conn);

        let impact = find_impact(&conn, "x").unwrap();
        let names: Vec<&str> = impact.iter().map(|s| s.name.as_str()).collect();
        // Transitive callers of x are y and z; the z→x and y→x edges must not
        // re-expand forever.
        assert_eq!(names, vec!["y", "z"]);
        // Idempotent under re-query (no growth from residual cycle state).
        let again = find_impact(&conn, "x").unwrap();
        assert_eq!(again.len(), impact.len());
    }
}
