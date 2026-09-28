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

/// Path of the current-directory project index (`./.keel/index.db`).
pub fn db_path() -> PathBuf {
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
/// 3. Daemon registry: project containing `cwd`, else sole registered index
///    (never guess among multiple unrelated registered projects)
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

    let existing: Vec<PathBuf> = roots
        .iter()
        .map(|root| project_index_db(root))
        .filter(|db| db.is_file())
        .collect();
    if existing.is_empty() {
        return None;
    }
    if existing.len() == 1 {
        return Some(existing.into_iter().next().unwrap());
    }
    // Multiple registered indexes and cwd matches none — do not guess by mtime
    // (wrong-repo High-confidence answers are worse than a fresh cwd index).
    if std::env::var_os("KEEL_MCP_DEBUG").is_some() {
        eprintln!(
            "keel: {} registered indexes and cwd is in none; falling back to cwd/.keel/index.db",
            existing.len()
        );
    }
    None
}

/// Open (creating the directory if needed) the on-disk index database.
fn open_db() -> Result<Connection> {
    open_db_at(Path::new("."))
}

/// Open the index database rooted at `root` (`<root>/.keel/index.db`).
///
/// The index always lives with the sources it describes: that keeps stored
/// (root-relative) paths truthful and stops query auto-index from wiping an
/// index that belongs to another directory.
fn open_db_at(root: &Path) -> Result<Connection> {
    let dir = root.join(DB_DIR);
    std::fs::create_dir_all(&dir).map_err(|source| KeelError::Io {
        path: dir.clone(),
        source,
    })?;
    // Best-effort: never fail indexing/queries over a gitignore write.
    let _ = ensure_gitignored(root);
    let conn = Connection::open(dir.join(DB_FILE))?;
    crate::db::configure_connection(&conn)?;
    Ok(conn)
}

/// Keep `<root>/.keel/` out of version control.
///
/// Appends `.keel/` to `<root>/.gitignore` when it isn't already covered, or
/// creates the file when `root` is a git checkout without one. Does nothing
/// outside a checkout. Returns whether the file was written.
pub fn ensure_gitignored(root: &Path) -> Result<bool> {
    const ENTRY: &str = ".keel/";
    let ignore_path = root.join(".gitignore");
    let existing = match std::fs::read_to_string(&ignore_path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if root.join(".git").exists() {
                std::fs::write(&ignore_path, format!("{ENTRY}\n")).map_err(|source| {
                    KeelError::Io {
                        path: ignore_path,
                        source,
                    }
                })?;
                return Ok(true);
            }
            return Ok(false);
        }
        Err(source) => {
            return Err(KeelError::Io {
                path: ignore_path,
                source,
            })
        }
    };
    let covered = existing.lines().any(|line| {
        let line = line.trim();
        matches!(line, ".keel/" | ".keel" | "/.keel/" | "/.keel")
    });
    if covered {
        return Ok(false);
    }
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&ignore_path)
        .map_err(|source| KeelError::Io {
            path: ignore_path.clone(),
            source,
        })?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        file.write_all(b"\n").map_err(|source| KeelError::Io {
            path: ignore_path.clone(),
            source,
        })?;
    }
    file.write_all(format!("{ENTRY}\n").as_bytes())
        .map_err(|source| KeelError::Io {
            path: ignore_path,
            source,
        })?;
    Ok(true)
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

/// Index the repository at `path` into `<path>/.keel/index.db`.
pub fn run_index(path: &Path) -> Result<index::IndexStats> {
    let mut conn = open_db_at(path)?;
    index::index_repository(path, &mut conn)
}

/// Watch the repository at `path` and re-index on changes until interrupted.
pub fn run_watch(path: &Path) -> Result<()> {
    let mut conn = open_db_at(path)?;
    index::watch::watch_repository(path, &mut conn)
}

/// Register `path` with the global daemon (indexes + watches the project).
pub fn run_start(path: &Path) -> Result<()> {
    crate::daemon::client_start_project(path)?;
    // The daemon indexes outside this process, so cover gitignore here.
    let _ = ensure_gitignored(path);
    Ok(())
}

/// Unregister the current project from the global daemon.
pub fn run_stop() -> Result<()> {
    crate::daemon::client_stop_project(Path::new("."))
}

/// Print global daemon + current project watch status.
pub fn run_status() -> Result<()> {
    crate::daemon::client_status(Path::new("."))
}

/// Stop the global daemon and its project watchers.
pub fn run_daemon_stop() -> Result<()> {
    crate::daemon::client_stop_daemon()
}

/// Diagnose daemon, project registration, and index health for `path`.
pub fn run_doctor(path: &Path) -> Result<()> {
    for check in crate::daemon::doctor_checks(path) {
        println!(
            "{}:\t{}\t{}",
            check.name,
            if check.ok { "ok" } else { "FAIL" },
            check.detail
        );
    }
    Ok(())
}

/// One-shot project setup: register with the daemon, then print MCP config.
///
/// The global daemon must already run (once per machine); this only fails
/// fast with the start command when it doesn't.
pub fn run_init(path: &Path) -> Result<()> {
    crate::daemon::client_start_project(path)?;
    let _ = ensure_gitignored(path);
    let exe = std::env::current_exe().map_err(|source| KeelError::Io {
        path: PathBuf::from("keel"),
        source,
    })?;
    println!("{}", mcp_config_snippet(&exe));
    Ok(())
}

/// Paste-ready MCP server config pointing at this `keel` binary.
pub fn mcp_config_snippet(exe: &Path) -> String {
    let config = serde_json::json!({
        "mcpServers": {
            "keel": {
                "command": exe.display().to_string(),
                "args": ["mcp"]
            }
        }
    });
    let body = serde_json::to_string_pretty(&config)
        .unwrap_or_else(|_| r#"{"mcpServers": {"keel": {"command": "keel", "args": ["mcp"]}}}"#.into());
    format!("Add this to Cursor Settings -> MCP (or ~/.cursor/mcp.json), then refresh MCP servers:\n{body}")
}

/// Miss notes worth repeating on human (non-JSON) CLI output.
///
/// Skips the canonical "No matching symbols found." marker (redundant with the
/// "No <query> found for <name>" line) and keeps recovery notes.
pub fn extra_miss_notes(notes: &[String]) -> Vec<&str> {
    notes
        .iter()
        .map(String::as_str)
        .filter(|n| *n != "No matching symbols found.")
        .collect()
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

/// Format a symbol hit as `path:line:col<tab>kind<tab>name` (1-based).
pub fn format_symbol_hit(s: &Symbol) -> String {
    format!(
        "{}:{}:{}\t{}\t{}",
        s.file.display(),
        s.start_line,
        s.start_col,
        s.kind.as_db(),
        s.name
    )
}

/// Format a reference/caller hit with the same shape as [`format_symbol_hit`].
pub fn format_reference_hit(r: &Reference) -> String {
    format!(
        "{}:{}:{}\t{}\t{}",
        r.file.display(),
        r.start_line,
        r.start_col,
        r.kind.as_db(),
        r.name
    )
}

/// Look up references by name.
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
    eprintln!("Keel Insights dashboard on http://{addr}/insights");
    api::serve(&addr, &db_path(), auto_index)
}

/// Serve the MCP stdio server against the best available index.
///
/// Resolution: `KEEL_INDEX_DB` if set; otherwise walk up from cwd for
/// `.keel/index.db`; otherwise use the daemon registry (project containing
/// cwd, or sole project); otherwise `cwd/.keel/index.db`.
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
    fn hit_formats_share_shape_with_kind() {
        use crate::graph::types::{ReferenceKind, SymbolKind};

        let sym = Symbol {
            name: "AuthService".into(),
            kind: SymbolKind::Struct,
            file: PathBuf::from("src/lib.rs"),
            start_line: 1,
            start_col: 12,
            module_path: "crate".into(),
        };
        let reference = Reference {
            name: "create_order".into(),
            file: PathBuf::from("src/main.rs"),
            start_line: 3,
            start_col: 5,
            kind: ReferenceKind::Call,
            container: String::new(),
        };
        assert_eq!(
            format_symbol_hit(&sym),
            "src/lib.rs:1:12\tstruct\tAuthService"
        );
        assert_eq!(
            format_reference_hit(&reference),
            "src/main.rs:3:5\tcall\tcreate_order"
        );
        // definition/references/callers/impact stay script-parseable as one shape.
        assert_eq!(
            format_symbol_hit(&sym).split('\t').count(),
            format_reference_hit(&reference).split('\t').count()
        );
    }

    #[test]
    fn index_writes_db_at_target_not_cwd() {
        let proj = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(proj.path().join("src")).unwrap();
        std::fs::write(proj.path().join("src/lib.rs"), "fn hello() {}\n").unwrap();

        // Indexing from another cwd must land beside the sources it describes.
        let stats = run_index(proj.path()).unwrap();
        assert_eq!(stats.indexed, 1);
        let db = proj.path().join(DB_DIR).join(DB_FILE);
        assert!(db.is_file());
        assert!(!proj.path().join(".gitignore").exists());

        // A second pass finds everything current and removes nothing (the old
        // cwd-relative layout wiped out-of-tree indexes here).
        let again = run_index(proj.path()).unwrap();
        assert_eq!(again.skipped, 1);
        assert_eq!(again.removed, 0);

        let conn = Connection::open(&db).unwrap();
        let defs = crate::db::queries::find_definition(&conn, "hello").unwrap();
        assert_eq!(defs.len(), 1);
        assert_eq!(defs[0].file, PathBuf::from("src/lib.rs"));
    }

    #[test]
    fn gitignore_appends_entry_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "target/\n").unwrap();
        assert!(ensure_gitignored(tmp.path()).unwrap());
        let text = std::fs::read_to_string(tmp.path().join(".gitignore")).unwrap();
        assert_eq!(text, "target/\n.keel/\n");
        // Second call is a no-op.
        assert!(!ensure_gitignored(tmp.path()).unwrap());
    }

    #[test]
    fn gitignore_respects_existing_coverage_and_newlines() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(".gitignore"), "target/\n.keel\n").unwrap();
        assert!(!ensure_gitignored(tmp.path()).unwrap());

        let tmp2 = tempfile::tempdir().unwrap();
        std::fs::write(tmp2.path().join(".gitignore"), "target/").unwrap();
        assert!(ensure_gitignored(tmp2.path()).unwrap());
        let text = std::fs::read_to_string(tmp2.path().join(".gitignore")).unwrap();
        assert_eq!(text, "target/\n.keel/\n");
    }

    #[test]
    fn mcp_snippet_points_at_given_binary() {
        let snippet = mcp_config_snippet(Path::new("/opt/keel/bin/keel"));
        assert!(snippet.contains("mcpServers"), "{snippet}");
        assert!(snippet.contains("/opt/keel/bin/keel"), "{snippet}");
        assert!(snippet.contains("\"mcp\""), "{snippet}");
    }

    #[test]
    fn extra_miss_notes_skips_canonical_marker() {
        let notes = vec![
            "No matching symbols found.".to_string(),
            "Did you mean `AuthService`?".to_string(),
        ];
        assert_eq!(extra_miss_notes(&notes), vec!["Did you mean `AuthService`?"]);
        assert!(extra_miss_notes(&[]).is_empty());
    }

    /// Point daemon discovery at a home and port with nothing behind them.
    fn isolate_daemon(home: &Path) {
        std::env::set_var("KEEL_HOME", home);
        std::env::set_var("KEEL_DAEMON_PORT", "9");
    }

    #[test]
    fn doctor_reports_empty_project_honestly() {
        let _guard = ENV_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        isolate_daemon(&home.path().join("keel-home"));
        let proj = tempfile::tempdir().unwrap();

        let checks = crate::daemon::doctor_checks(proj.path());
        let get = |name: &str| {
            checks
                .iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("missing {name} check"))
        };
        assert!(get("version").ok);
        assert!(!get("daemon").ok);
        assert!(get("daemon").detail.contains("start it with"));
        assert!(!get("project").ok);
        assert!(get("project").detail.contains("not registered"));
        assert!(!get("index").ok);
        assert!(get("index").detail.contains("missing"));

        std::env::remove_var("KEEL_HOME");
        std::env::remove_var("KEEL_DAEMON_PORT");
    }

    #[test]
    fn doctor_sees_index_and_dead_watcher() {
        let _guard = ENV_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let keel_home = home.path().join("keel-home");
        isolate_daemon(&keel_home);
        let proj = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(proj.path().join("src")).unwrap();
        std::fs::write(proj.path().join("src/lib.rs"), "fn hello() {}\n").unwrap();

        // Real index on disk.
        let db_dir = proj.path().join(DB_DIR);
        std::fs::create_dir_all(&db_dir).unwrap();
        let mut conn =
            rusqlite::Connection::open(db_dir.join(DB_FILE)).unwrap();
        crate::index::index_repository(proj.path(), &mut conn).unwrap();
        drop(conn);

        // Registry entry with a genuinely dead pid: spawn and reap a child so
        // the pid is valid-range but guaranteed defunct.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();
        let canon = std::fs::canonicalize(proj.path()).unwrap();
        let daemon_dir = keel_home.join("daemon");
        std::fs::create_dir_all(&daemon_dir).unwrap();
        std::fs::write(
            daemon_dir.join("projects.json"),
            format!(
                r#"{{"projects":[{{"path":"{}","pid":{dead_pid}}}]}}"#,
                canon.display(),
            ),
        )
        .unwrap();

        let checks = crate::daemon::doctor_checks(proj.path());
        let get = |name: &str| checks.iter().find(|c| c.name == name).unwrap();
        assert!(get("index").ok, "index: {}", get("index").detail);
        assert!(get("index").detail.contains("1 file(s)"));
        assert!(!get("project").ok);
        assert!(get("project").detail.contains("dead"));

        std::env::remove_var("KEEL_HOME");
        std::env::remove_var("KEEL_DAEMON_PORT");
    }

    #[test]
    fn init_fails_fast_without_daemon() {
        let _guard = ENV_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        isolate_daemon(&home.path().join("keel-home"));
        let proj = tempfile::tempdir().unwrap();

        let err = run_init(proj.path()).unwrap_err();
        assert!(
            err.to_string().contains("keel daemon is not running"),
            "unexpected error: {err}"
        );

        std::env::remove_var("KEEL_HOME");
        std::env::remove_var("KEEL_DAEMON_PORT");
    }

    #[test]
    fn daemon_stop_is_ok_when_down() {
        let _guard = ENV_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        isolate_daemon(&home.path().join("keel-home"));

        run_daemon_stop().unwrap();

        std::env::remove_var("KEEL_HOME");
        std::env::remove_var("KEEL_DAEMON_PORT");
    }

    #[test]
    fn gitignore_created_only_inside_checkouts() {
        let checkout = tempfile::tempdir().unwrap();
        std::fs::create_dir(checkout.path().join(".git")).unwrap();
        assert!(ensure_gitignored(checkout.path()).unwrap());
        let text = std::fs::read_to_string(checkout.path().join(".gitignore")).unwrap();
        assert_eq!(text, ".keel/\n");

        let plain = tempfile::tempdir().unwrap();
        assert!(!ensure_gitignored(plain.path()).unwrap());
        assert!(!plain.path().join(".gitignore").exists());
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
    fn resolve_ambiguous_registry_does_not_guess() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var("KEEL_INDEX_DB");
        let home = tempfile::tempdir().unwrap();
        let a = home.path().join("proj-a");
        let b = home.path().join("proj-b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        write_empty_db(&a);
        write_empty_db(&b);
        let daemon = home.path().join(".keel").join("daemon");
        std::fs::create_dir_all(&daemon).unwrap();
        std::fs::write(
            daemon.join("projects.json"),
            format!(
                r#"{{"projects":[{{"path":"{}","pid":1}},{{"path":"{}","pid":2}}]}}"#,
                a.display(),
                b.display()
            ),
        )
        .unwrap();
        std::env::set_var("KEEL_HOME", home.path().join(".keel"));
        let cwd = home.path().join("elsewhere");
        std::fs::create_dir_all(&cwd).unwrap();
        let got = resolve_index_db(&cwd);
        std::env::remove_var("KEEL_HOME");
        assert_eq!(
            got,
            project_index_db(&cwd),
            "must not pick an unrelated registered project by mtime"
        );
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

