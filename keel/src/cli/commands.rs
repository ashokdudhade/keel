//! Execution logic behind each CLI subcommand.

use crate::api;
use crate::db::schema;
use crate::error::{Result, KeelError};
use crate::graph::deps::Dependency;
use crate::graph::types::{ImplRecord, Reference, Symbol};
use crate::index::{self, IndexStats};
use crate::mcp;
use rusqlite::Connection;
use std::path::{Path, PathBuf};

const DB_DIR: &str = ".keel";
const DB_FILE: &str = "index.db";

fn db_path() -> PathBuf {
    Path::new(DB_DIR).join(DB_FILE)
}

fn project_index_db(root: &Path) -> PathBuf {
    root.join(DB_DIR).join(DB_FILE)
}

/// Resolve which on-disk index MCP (and similar callers) should open.
///
/// Priority:
/// 1. `KEEL_INDEX_DB` when set
/// 2. Walk up from `cwd` for an existing `.keel/index.db`
/// 3. Daemon registry: project containing `cwd`, else sole registered index,
///    else most recently modified registered index
/// 4. Fallback: `cwd/.keel/index.db` (may be created on first use)
pub fn resolve_index_db(cwd: &Path) -> PathBuf {
    if let Some(p) = std::env::var_os("KEEL_INDEX_DB") {
        return PathBuf::from(p);
    }
    if let Some(found) = find_index_walking_up(cwd) {
        return found;
    }
    if let Some(found) = find_index_from_registry(cwd) {
        return found;
    }
    cwd.join(DB_DIR).join(DB_FILE)
}

fn find_index_walking_up(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        let candidate = project_index_db(&dir);
        if candidate.is_file() {
            return Some(candidate);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn find_index_from_registry(cwd: &Path) -> Option<PathBuf> {
    let roots = crate::daemon::registered_project_roots();
    if roots.is_empty() {
        return None;
    }

    let cwd_canon = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut best_prefix: Option<(usize, PathBuf)> = None;
    for root in &roots {
        let root_canon = root.canonicalize().unwrap_or_else(|_| root.clone());
        if cwd_canon.starts_with(&root_canon) {
            let db = project_index_db(&root_canon);
            if db.is_file() {
                let score = root_canon.as_os_str().len();
                if best_prefix
                    .as_ref()
                    .map(|(s, _)| score > *s)
                    .unwrap_or(true)
                {
                    best_prefix = Some((score, db));
                }
            }
        }
    }
    if let Some((_, db)) = best_prefix {
        return Some(db);
    }

    let mut existing: Vec<(PathBuf, std::time::SystemTime)> = roots
        .iter()
        .map(|root| project_index_db(root))
        .filter(|db| db.is_file())
        .filter_map(|db| {
            let modified = std::fs::metadata(&db).ok()?.modified().ok()?;
            Some((db, modified))
        })
        .collect();
    if existing.is_empty() {
        return None;
    }
    if existing.len() == 1 {
        return Some(existing.remove(0).0);
    }
    existing.sort_by_key(|(_, m)| *m);
    existing.pop().map(|(db, _)| db)
}

/// Open (creating the directory if needed) the on-disk index database.
fn open_db() -> Result<Connection> {
    std::fs::create_dir_all(DB_DIR)
        .map_err(|source| KeelError::Io {
            path: PathBuf::from(DB_DIR),
            source,
        })?;
    let conn = Connection::open(db_path())?;
    crate::db::configure_connection(&conn)?;
    Ok(conn)
}

/// Run a fast incremental index of `root` into the project DB.
///
/// Unchanged files are hash-skipped. Emits a one-line stderr note only when
/// something actually changed or failed.
pub fn ensure_index(root: &Path) -> Result<IndexStats> {
    let mut conn = open_db()?;
    let stats = index::index_repository(root, &mut conn)?;
    if stats.indexed + stats.removed + stats.errors > 0 {
        eprintln!(
            "keel: auto-indexed {} file(s) (skipped {}, removed {}, errors {}).",
            stats.indexed, stats.skipped, stats.removed, stats.errors
        );
    }
    Ok(stats)
}

fn maybe_ensure_index(auto_index: bool) -> Result<()> {
    if auto_index {
        ensure_index(Path::new("."))?;
    }
    Ok(())
}

/// Index the repository at `path`. Returns incremental indexing stats.
pub fn run_index(path: &Path) -> Result<index::IndexStats> {
    let mut conn = open_db()?;
    index::index_repository(path, &mut conn)
}

/// Watch the repository at `path` and re-index on changes until interrupted.
pub fn run_watch(path: &Path) -> Result<()> {
    let mut conn = open_db()?;
    index::watch::watch_repository(path, &mut conn)
}

/// Register `path` with the global daemon (indexes + watches the project).
pub fn run_start(path: &Path) -> Result<()> {
    crate::daemon::client_start_project(path)
}

/// Unregister the current project from the global daemon.
pub fn run_stop() -> Result<()> {
    crate::daemon::client_stop_project(Path::new("."))
}

/// Print global daemon + current project watch status.
pub fn run_status() -> Result<()> {
    crate::daemon::client_status(Path::new("."))
}

/// Run the global daemon (brew services).
pub fn run_daemon(port: u16) -> Result<()> {
    crate::daemon::run_daemon(port)
}

/// Look up definitions by name.
pub fn run_definition(name: &str, auto_index: bool) -> Result<Vec<Symbol>> {
    Ok(run_definition_meta(name, auto_index)?.results)
}

/// Definitions with confidence metadata.
pub fn run_definition_meta(
    name: &str,
    auto_index: bool,
) -> Result<crate::graph::query_result::QueryResult<Symbol>> {
    maybe_ensure_index(auto_index)?;
    let conn = open_db()?;
    schema::initialize(&conn)?;
    crate::facade::definition_with_meta(&conn, name)
}

/// Look up references by name (also used for `callers` in v0.1).
pub fn run_references(name: &str, auto_index: bool) -> Result<Vec<Reference>> {
    Ok(run_references_meta(name, auto_index)?.results)
}

/// References with confidence metadata.
pub fn run_references_meta(
    name: &str,
    auto_index: bool,
) -> Result<crate::graph::query_result::QueryResult<Reference>> {
    maybe_ensure_index(auto_index)?;
    let conn = open_db()?;
    schema::initialize(&conn)?;
    crate::facade::references_with_meta(&conn, name)
}

/// Look up callers of `name` with import-aware precision when a unique
/// definition module can be determined; otherwise falls back to all sites.
pub fn run_callers(name: &str, auto_index: bool) -> Result<Vec<Reference>> {
    Ok(run_callers_meta(name, auto_index)?.results)
}

/// Callers with confidence metadata.
pub fn run_callers_meta(
    name: &str,
    auto_index: bool,
) -> Result<crate::graph::query_result::QueryResult<Reference>> {
    maybe_ensure_index(auto_index)?;
    let conn = open_db()?;
    schema::initialize(&conn)?;
    crate::facade::callers_with_meta(&conn, name)
}

/// Look up trait implementations by trait name.
pub fn run_implementations(trait_name: &str, auto_index: bool) -> Result<Vec<ImplRecord>> {
    Ok(run_implementations_meta(trait_name, auto_index)?.results)
}

/// Implementations with confidence metadata.
pub fn run_implementations_meta(
    trait_name: &str,
    auto_index: bool,
) -> Result<crate::graph::query_result::QueryResult<ImplRecord>> {
    maybe_ensure_index(auto_index)?;
    let conn = open_db()?;
    schema::initialize(&conn)?;
    crate::facade::implementations_with_meta(&conn, trait_name)
}

/// Look up modules/files that `name` (module path or symbol) depends on.
pub fn run_dependencies(name: &str, auto_index: bool) -> Result<Vec<Dependency>> {
    Ok(run_dependencies_meta(name, auto_index)?.results)
}

/// Dependencies with confidence metadata.
pub fn run_dependencies_meta(
    name: &str,
    auto_index: bool,
) -> Result<crate::graph::query_result::QueryResult<Dependency>> {
    maybe_ensure_index(auto_index)?;
    let conn = open_db()?;
    schema::initialize(&conn)?;
    crate::facade::dependencies_with_meta(&conn, name)
}

/// Look up symbols transitively impacted by changing `name`.
pub fn run_impact(name: &str, auto_index: bool) -> Result<Vec<Symbol>> {
    Ok(run_impact_meta(name, auto_index)?.results)
}

/// Impact with confidence metadata.
pub fn run_impact_meta(
    name: &str,
    auto_index: bool,
) -> Result<crate::graph::query_result::QueryResult<Symbol>> {
    maybe_ensure_index(auto_index)?;
    let conn = open_db()?;
    schema::initialize(&conn)?;
    crate::facade::impact_with_meta(&conn, name)
}

/// Serve the JSON API on `127.0.0.1:{port}` using the on-disk index.
pub fn run_serve(port: u16, auto_index: bool) -> Result<()> {
    if auto_index {
        ensure_index(Path::new("."))?;
    } else {
        let conn = open_db()?;
        schema::initialize(&conn)?;
        drop(conn);
    }
    let addr = format!("127.0.0.1:{port}");
    eprintln!("Serving Keel JSON API on http://{addr}");
    api::serve(&addr, &db_path(), auto_index)
}

/// Serve the MCP stdio server against the best available index.
///
/// Resolution: `KEEL_INDEX_DB` if set; otherwise walk up from cwd for
/// `.keel/index.db`; otherwise use the daemon registry (project containing
/// cwd, sole project, or most recently modified index); otherwise
/// `cwd/.keel/index.db`.
pub fn run_mcp(auto_index: bool) -> Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let db = resolve_index_db(&cwd);
    if let Some(parent) = db.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|source| KeelError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
    }
    // Stay quiet on stderr by default — Cursor surfaces stderr as MCP errors.
    if std::env::var_os("KEEL_MCP_DEBUG").is_some() {
        eprintln!("Serving Keel MCP on stdio (db={})", db.display());
    }
    mcp::serve(&db, auto_index)
}

/// Project root that owns a `.keel/index.db` path.
pub fn index_root_from_db(db_path: &Path) -> PathBuf {
    db_path
        .parent()
        .and_then(|keel_dir| keel_dir.parent())
        .map(|p| {
            if p.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                p.to_path_buf()
            }
        })
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn write_empty_db(root: &Path) {
        let dir = root.join(DB_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(DB_FILE), b"").unwrap();
    }

    #[test]
    fn resolve_prefers_keel_index_db_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let override_db = tmp.path().join("custom.db");
        std::fs::write(&override_db, b"").unwrap();
        std::env::set_var("KEEL_INDEX_DB", &override_db);
        let got = resolve_index_db(tmp.path());
        std::env::remove_var("KEEL_INDEX_DB");
        assert_eq!(got, override_db);
    }

    #[test]
    fn resolve_walks_up_to_existing_index() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("KEEL_INDEX_DB");
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("proj");
        let nested = project.join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        write_empty_db(&project);
        let got = resolve_index_db(&nested);
        assert_eq!(got, project_index_db(&project));
    }

    #[test]
    fn resolve_uses_sole_registered_project() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("KEEL_INDEX_DB");
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join("only-proj");
        std::fs::create_dir_all(&project).unwrap();
        write_empty_db(&project);
        let daemon = home.path().join(".keel").join("daemon");
        std::fs::create_dir_all(&daemon).unwrap();
        std::fs::write(
            daemon.join("projects.json"),
            format!(
                r#"{{"projects":[{{"path":"{}","pid":1}}]}}"#,
                project.display()
            ),
        )
        .unwrap();
        std::env::set_var("KEEL_HOME", home.path().join(".keel"));
        let cwd = home.path().join("elsewhere");
        std::fs::create_dir_all(&cwd).unwrap();
        let got = resolve_index_db(&cwd);
        std::env::remove_var("KEEL_HOME");
        assert_eq!(got, project_index_db(&project));
    }

    #[test]
    fn resolve_falls_back_to_cwd_index_path() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("KEEL_INDEX_DB");
        let home = tempfile::tempdir().unwrap();
        std::env::set_var("KEEL_HOME", home.path().join("empty-home"));
        let cwd = home.path().join("fresh");
        std::fs::create_dir_all(&cwd).unwrap();
        let got = resolve_index_db(&cwd);
        std::env::remove_var("KEEL_HOME");
        assert_eq!(got, project_index_db(&cwd));
    }
}

