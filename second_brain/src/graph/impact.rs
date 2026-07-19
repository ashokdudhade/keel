//! Transitive impact analysis: who (directly or indirectly) references a name.

use crate::db::queries;
use crate::error::Result;
use crate::graph::resolve;
use crate::graph::types::Symbol;
use rusqlite::Connection;
use std::collections::{BTreeSet, HashSet};
use std::path::Path;

/// Return the transitive set of symbols that reference `name`.
///
/// Expansion uses qualified identities (`module_path::name` when module is
/// non-empty). References are accepted only when
/// [`resolve::resolve_definition_ranked`] from the reference's file yields an
/// [`resolve::acceptable_top_match`] whose identity equals the worklist target.
/// Results are ordered by `(name, path, line, col)` and de-duplicated by symbol
/// identity `(name, module_path, path, line, col)`.
pub fn find_impact(conn: &Connection, name: &str) -> Result<Vec<Symbol>> {
    let mut visited: HashSet<String> = HashSet::new();
    let mut worklist: BTreeSet<String> = BTreeSet::new();

    let defs = queries::find_definition(conn, name)?;
    if defs.is_empty() {
        let id = name.to_string();
        visited.insert(id.clone());
        worklist.insert(id);
    } else {
        for d in &defs {
            let id = symbol_identity(d);
            visited.insert(id.clone());
            worklist.insert(id);
        }
    }

    let mut impact: Vec<Symbol> = Vec::new();
    let mut seen_symbols: HashSet<(String, String, String, u32, u32)> = HashSet::new();

    while let Some(current_id) = pop_front(&mut worklist) {
        let current_name = bare_name(&current_id);
        for reference in queries::find_references(conn, current_name)? {
            let from = reference.file.to_string_lossy();
            let ranked = resolve::resolve_definition_ranked(conn, current_name, from.as_ref())?;
            if !resolves_to_target(&ranked, &current_id) {
                continue;
            }

            let container = reference.container.as_str();
            if container.is_empty() || visited.contains(container) {
                continue;
            }
            visited.insert(container.to_string());
            worklist.insert(container.to_string());

            push_container_symbols(conn, container, &mut impact, &mut seen_symbols)?;
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

fn push_container_symbols(
    conn: &Connection,
    container: &str,
    impact: &mut Vec<Symbol>,
    seen_symbols: &mut HashSet<(String, String, String, u32, u32)>,
) -> Result<()> {
    // Qualified container: prefer module + bare name lookup.
    if let Some((module, name)) = container.rsplit_once("::") {
        if !looks_like_file_path(module) {
            let qualified = queries::find_definition_by_qualified(conn, module, name)?;
            if !qualified.is_empty() {
                for sym in qualified {
                    insert_impact_symbol(sym, impact, seen_symbols);
                }
                return Ok(());
            }
            for sym in queries::find_definition(conn, name)? {
                if symbol_identity(&sym) == container {
                    insert_impact_symbol(sym, impact, seen_symbols);
                }
            }
            return Ok(());
        }
    }

    if looks_like_file_path(container) {
        // File-path container (empty extraction scope): no single symbol to add.
        return Ok(());
    }

    let bare = bare_name(container);
    for sym in queries::find_definition(conn, bare)? {
        insert_impact_symbol(sym, impact, seen_symbols);
    }
    Ok(())
}

fn insert_impact_symbol(
    sym: Symbol,
    impact: &mut Vec<Symbol>,
    seen_symbols: &mut HashSet<(String, String, String, u32, u32)>,
) {
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

fn symbol_identity(sym: &Symbol) -> String {
    if sym.module_path.is_empty() {
        sym.name.clone()
    } else {
        format!("{}::{}", sym.module_path, sym.name)
    }
}

fn bare_name(identity: &str) -> &str {
    identity.rsplit("::").next().unwrap_or(identity)
}

fn looks_like_file_path(s: &str) -> bool {
    s.contains('/') || s.contains('\\') || Path::new(s).extension().is_some()
}

fn resolves_to_target(ranked: &[(u8, Symbol)], target_id: &str) -> bool {
    let Some(sym) = resolve::acceptable_top_match(ranked) else {
        return false;
    };
    let id = symbol_identity(sym);
    if id == target_id {
        return true;
    }
    // Bare-name seed (no definitions found): accept precise/unique defs of that name.
    if !target_id.contains("::") && sym.name == target_id {
        return true;
    }
    false
}

fn pop_front(worklist: &mut BTreeSet<String>) -> Option<String> {
    let next = worklist.iter().next()?.clone();
    worklist.remove(&next);
    Some(next)
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
                    container: "crate::b".into(),
                },
                Reference {
                    name: "b".into(),
                    file: PathBuf::new(),
                    start_line: 3,
                    start_col: 10,
                    kind: ReferenceKind::Call,
                    container: "crate::c".into(),
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
                    container: "crate::y".into(),
                },
                Reference {
                    name: "y".into(),
                    file: PathBuf::new(),
                    start_line: 3,
                    start_col: 10,
                    kind: ReferenceKind::Call,
                    container: "crate::z".into(),
                },
                Reference {
                    name: "z".into(),
                    file: PathBuf::new(),
                    start_line: 1,
                    start_col: 10,
                    kind: ReferenceKind::Call,
                    container: "crate::x".into(),
                },
                Reference {
                    name: "y".into(),
                    file: PathBuf::new(),
                    start_line: 1,
                    start_col: 20,
                    kind: ReferenceKind::Call,
                    container: "crate::x".into(),
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
