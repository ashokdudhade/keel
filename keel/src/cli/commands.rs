//! Execution logic behind each CLI subcommand.

use crate::api;
use crate::db::schema;
use crate::error::{Result, KeelError};
use crate::graph::deps::Dependency;
use crate::graph::types::{ImplRecord, Reference, Symbol};
use crate::index::{self, IndexStats};
use crate::mcp;
use rusqlite::Connection;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const DB_DIR: &str = ".keel";
const DB_FILE: &str = "index.db";

/// Root whose index CLI queries read: nearest ancestor (or cwd itself) with
/// an existing `.keel/index.db`, else cwd. Lets queries run from project
/// subdirectories without stranding them on a fresh empty index.
///
/// Explicit-scope commands (`index`/`watch`/daemon) keep their given path;
/// only the implicit query root resolves upward. Unlike MCP resolution
/// this ignores `KEEL_INDEX_DB` and the daemon registry: a globally
/// exported override must not hijack local queries.
pub fn query_root() -> PathBuf {
    // Absolute: `find_index_walking_up` cannot ascend from `"."` (`pop`
    // fails), so resolve the real cwd first.
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if let Some(db) = find_index_walking_up(&cwd) {
        if let Some(root) = db.parent().and_then(|d| d.parent()) {
            return root.to_path_buf();
        }
    }
    PathBuf::from(".")
}

/// Path of the project index queries read (see [`query_root`]).
pub fn db_path() -> PathBuf {
    project_index_db(&query_root())
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
    open_db_at(&query_root())
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
    let mut conn = open_db_at(root)?;
    let stats = index::index_repository(root, &mut conn)?;
    if stats.indexed + stats.removed + stats.errors > 0 {
        eprintln!(
            "keel: auto-indexed {} file(s) (skipped {}, removed {}, errors {}, syntax errors {}).",
            stats.indexed,
            stats.skipped,
            stats.removed,
            stats.errors,
            stats.syntax_errors
        );
    }
    Ok(stats)
}

fn maybe_ensure_index(auto_index: bool) -> Result<()> {
    if auto_index {
        ensure_index(&query_root())?;
    }
    Ok(())
}

/// Reject index/watch roots that are not existing directories, before
/// anything (like `<root>/.keel/`) is created there. A typo'd path must
/// fail loudly, not "succeed" with zero files.
pub fn require_indexable_dir(path: &Path) -> Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    let reason = if path.exists() {
        "not a directory"
    } else {
        "no such directory"
    };
    Err(KeelError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(std::io::ErrorKind::NotFound, reason),
    })
}

/// Index the repository at `path` into `<path>/.keel/index.db`.
pub fn run_index(path: &Path) -> Result<index::IndexStats> {
    require_indexable_dir(path)?;
    let mut conn = open_db_at(path)?;
    index::index_repository(path, &mut conn)
}

/// Print one human `Indexed …` summary line (single source of truth for
/// the stats format, shared by `index` and `init`).
pub fn print_index_stats(stats: &index::IndexStats) {
    println!(
        "Indexed {} file(s) (skipped {}, removed {}, errors {}, syntax errors {}).",
        stats.indexed,
        stats.skipped,
        stats.removed,
        stats.errors,
        stats.syntax_errors
    );
}

/// Watch the repository at `path` and re-index on changes until interrupted.
pub fn run_watch(path: &Path) -> Result<()> {
    require_indexable_dir(path)?;
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

/// Print global daemon + current project watch status, plus Insights server.
pub fn run_status() -> Result<()> {
    run_status_at(Path::new("."))
}

fn run_status_at(path: &Path) -> Result<()> {
    crate::daemon::client_status(path)?;
    match insights_status(path) {
        InsightsStatus::Running { port, api_error } => {
            if let Some(e) = api_error {
                println!(
                    "insights:\trunning on port {port} (UNHEALTHY: {e} — run: keel insights-stop)"
                );
            } else {
                println!("insights:\trunning on port {port}");
            }
        }
        InsightsStatus::Stopped => println!("insights:\tstopped"),
    }
    Ok(())
}

/// Status as JSON (`{"daemon": {...}, "insights": {state[, port[, api_error]]}}`).
pub fn status_json(path: &Path) -> Result<String> {
    let daemon = crate::daemon::daemon_status(path)?;
    let insights = match insights_status(path) {
        InsightsStatus::Running { port, api_error } => {
            let mut obj = serde_json::json!({"state": "running", "port": port});
            if let Some(e) = api_error {
                obj["api_error"] = serde_json::json!(e);
            }
            obj
        }
        InsightsStatus::Stopped => serde_json::json!({"state": "stopped"}),
    };
    serde_json::to_string_pretty(&serde_json::json!({"daemon": daemon, "insights": insights}))
        .map_err(|e| KeelError::Watch(e.to_string()))
}

/// Insights server state for `status`.
#[derive(Debug, PartialEq, Eq)]
enum InsightsStatus {
    Running {
        port: u16,
        api_error: Option<String>,
    },
    Stopped,
}

/// This project's Insights server state from its marker + `/health`, plus a
/// best-effort `/api/insights` error probe (stale servers answer `/health`
/// fine while every data endpoint fails).
fn insights_status(root: &Path) -> InsightsStatus {
    let port_file = root.join(DB_DIR).join(INSIGHTS_PORT_FILE);
    match read_insights_addr(&port_file) {
        Some((port, _)) if server_healthy(port) => InsightsStatus::Running {
            port,
            api_error: server_api_error(port),
        },
        _ => InsightsStatus::Stopped,
    }
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

/// Doctor checks as JSON (`{"checks": [{name, ok, detail}, …]}`).
pub fn doctor_checks_json(path: &Path) -> Result<String> {
    let checks = crate::daemon::doctor_checks(path);
    serde_json::to_string_pretty(&serde_json::json!({ "checks": checks }))
        .map_err(|e| KeelError::Watch(e.to_string()))
}

/// One-shot project setup: index now, then print MCP config.
///
/// Never needs the daemon: when one is already running the project is also
/// registered for live watching, otherwise a hint explains the follow-up.
/// (Indexing via [`run_index`] also creates `.keel/` and covers gitignore.)
pub fn run_init(path: &Path) -> Result<()> {
    let stats = run_index(path)?;
    print_index_stats(&stats);
    if crate::daemon::daemon_reachable() {
        crate::daemon::client_start_project(path)?;
    } else {
        println!(
            "keel daemon is not running; for live re-indexing, start it with `{}` then run `keel start`.",
            crate::daemon::daemon_start_hint()
        );
    }
    let exe = std::env::current_exe().map_err(|source| KeelError::Io {
        path: PathBuf::from("keel"),
        source,
    })?;
    println!("{}", mcp_config_snippet(&exe, path));
    Ok(())
}

/// Paste-ready MCP server config pointing at this `keel` binary.
///
/// Covers Cursor (JSON block) and Claude Code (CLI command); other MCP
/// clients accept the same command + args pair. Both pin the project root:
/// MCP clients may spawn servers with any cwd (global installs), and an
/// unpinned `keel mcp` would resolve the wrong index — or none — instead
/// of this project's.
/// Best-effort absolute form of `path` for config snippets.
/// Canonicalized when the path exists (no `/./` warts, symlinks resolved
/// like the daemon registry expects), else joined onto the cwd.
fn absolutize(path: &Path) -> PathBuf {
    if let Ok(canon) = std::fs::canonicalize(path) {
        return canon;
    }
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

pub fn mcp_config_snippet(exe: &Path, project_root: &Path) -> String {
    let abs_root = absolutize(project_root);
    let index_db = abs_root.join(DB_DIR).join(DB_FILE);
    let config = serde_json::json!({
        "mcpServers": {
            "keel": {
                "command": exe.display().to_string(),
                "args": ["mcp"],
                "cwd": abs_root.display().to_string(),
                "env": {
                    "KEEL_INDEX_DB": index_db.display().to_string()
                }
            }
        }
    });
    let body = serde_json::to_string_pretty(&config)
        .unwrap_or_else(|_| r#"{"mcpServers": {"keel": {"command": "keel", "args": ["mcp"]}}}"#.into());
    format!(
        "Add this to Cursor Settings -> MCP (or ~/.cursor/mcp.json), then refresh MCP servers:\n{body}\n\nOr register with Claude Code:\nclaude mcp add keel -e KEEL_INDEX_DB={} -- {} mcp",
        index_db.display(),
        exe.display()
    )
}

/// True when impact notes bridge file-scope uses: no impacted symbols,
/// but the target is used at top level somewhere.
pub fn has_file_scope_note(notes: &[String]) -> bool {
    notes.iter().any(|n| n.starts_with("Also used at file scope in "))
}

/// Miss notes worth repeating on human (non-JSON) CLI output.
///
/// Skips miss markers redundant with the "No <query> found for <name>" header
/// (the canonical marker and search's pattern-specific one) and keeps
/// recovery notes.
pub fn extra_miss_notes(notes: &[String]) -> Vec<&str> {
    notes
        .iter()
        .map(String::as_str)
        .filter(|n| *n != "No matching symbols found." && !n.starts_with("No symbols matching `"))
        .collect()
}

/// Run the global daemon (brew services).
pub fn run_daemon(port: u16) -> Result<()> {
    crate::daemon::run_daemon(port)
}

/// Look up definitions by name.
pub fn run_definition(name: &str, auto_index: bool) -> Result<Vec<Symbol>> {
    Ok(run_definition_meta(name, None, auto_index)?.results)
}

/// Definitions with confidence metadata.
pub fn run_definition_meta(
    name: &str,
    module: Option<&str>,
    auto_index: bool,
) -> Result<crate::graph::query_result::QueryResult<Symbol>> {
    maybe_ensure_index(auto_index)?;
    let conn = open_db()?;
    schema::initialize(&conn)?;
    crate::facade::definition_with_meta_opts(&conn, name, module)
}

/// Outline (symbols defined in a file) with confidence metadata.
pub fn run_outline_meta(
    file: &str,
    auto_index: bool,
) -> Result<crate::graph::query_result::QueryResult<Symbol>> {
    maybe_ensure_index(auto_index)?;
    let conn = open_db()?;
    schema::initialize(&conn)?;
    crate::facade::outline_with_meta(&conn, file)
}

/// Unreferenced-function sweep with confidence metadata (`None` = whole
/// project, else a file, module, or directory target).
pub fn run_unused_meta(
    target: Option<&str>,
    auto_index: bool,
) -> Result<crate::graph::query_result::QueryResult<Symbol>> {
    run_unused_meta_opts(target, false, auto_index)
}

/// Unused sweep with an optional transitive pass over references from
/// candidate-dead functions.
pub fn run_unused_meta_opts(
    target: Option<&str>,
    transitive: bool,
    auto_index: bool,
) -> Result<crate::graph::query_result::QueryResult<Symbol>> {
    maybe_ensure_index(auto_index)?;
    let conn = open_db()?;
    schema::initialize(&conn)?;
    crate::facade::unused_with_meta_opts(&conn, target, transitive)
}

/// Substring symbol search with confidence metadata.
pub fn run_search_meta(
    pattern: &str,
    limit: usize,
    auto_index: bool,
) -> Result<crate::graph::query_result::QueryResult<Symbol>> {
    maybe_ensure_index(auto_index)?;
    let conn = open_db()?;
    schema::initialize(&conn)?;
    crate::facade::search_with_meta(&conn, pattern, limit)
}

/// Maximum source characters in one `--preview` line; longer lines
/// (minified files) truncate with a trailing `…`.
pub const PREVIEW_MAX_CHARS: usize = 200;

/// Source-line cache for `--preview`, rooted at the same index queries use.
/// Each project file is read at most once per invocation; missing files and
/// out-of-range lines yield `None`, so stale index rows preview as nothing
/// instead of failing the query.
pub struct PreviewCache {
    root: PathBuf,
    files: HashMap<PathBuf, Option<String>>,
}

impl PreviewCache {
    /// Cache rooted at `root`, against which indexed relative paths resolve.
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            files: HashMap::new(),
        }
    }

    /// 1-based source `line` of indexed relative `file`, stripped of `\r`
    /// and a leading BOM and truncated to [`PREVIEW_MAX_CHARS`].
    pub fn line(&mut self, file: &Path, line: u32) -> Option<String> {
        let text = self
            .files
            .entry(file.to_path_buf())
            .or_insert_with(|| std::fs::read_to_string(self.root.join(file)).ok())
            .as_ref()?;
        let raw = text.lines().nth(line.checked_sub(1)? as usize)?;
        let clean = raw.strip_prefix('\u{FEFF}').unwrap_or(raw);
        Some(truncate_preview(clean))
    }
}

/// Preview cache for this invocation when `preview` was requested, rooted
/// at the resolved index so previews match the queried DB.
pub fn preview_cache(preview: bool) -> Option<PreviewCache> {
    if !preview {
        return None;
    }
    Some(PreviewCache::new(index_root_from_db(&db_path())))
}

fn truncate_preview(s: &str) -> String {
    if s.chars().count() > PREVIEW_MAX_CHARS {
        let mut out: String = s.chars().take(PREVIEW_MAX_CHARS).collect();
        out.push('…');
        out
    } else {
        s.to_string()
    }
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

/// Format an implementation hit as `path:line:col<tab>type` (1-based).
pub fn format_impl_hit(i: &ImplRecord) -> String {
    format!(
        "{}:{}:{}\t{}",
        i.file.display(),
        i.start_line,
        i.start_col,
        i.type_name
    )
}

/// Print one symbol hit plus its `--preview` source line when `previews`
/// is active. The preview is indented, so script parsers keep matching hit
/// rows by "no leading whitespace".
pub fn print_symbol_hit(s: &Symbol, previews: &mut Option<PreviewCache>) {
    println!("{}", format_symbol_hit(s));
    if let Some(cache) = previews.as_mut() {
        if let Some(p) = cache.line(&s.file, s.start_line) {
            println!("    {p}");
        }
    }
}

/// Print one reference hit plus its `--preview` source line (see
/// [`print_symbol_hit`]).
pub fn print_reference_hit(r: &Reference, previews: &mut Option<PreviewCache>) {
    println!("{}", format_reference_hit(r));
    if let Some(cache) = previews.as_mut() {
        if let Some(p) = cache.line(&r.file, r.start_line) {
            println!("    {p}");
        }
    }
}

/// Print one implementation hit plus its `--preview` source line (see
/// [`print_symbol_hit`]).
pub fn print_impl_hit(i: &ImplRecord, previews: &mut Option<PreviewCache>) {
    println!("{}", format_impl_hit(i));
    if let Some(cache) = previews.as_mut() {
        if let Some(p) = cache.line(&i.file, i.start_line) {
            println!("    {p}");
        }
    }
}

/// Look up references by name (uncapped: the plain `Vec` API cannot carry
/// a truncation note, so it never truncates).
pub fn run_references(name: &str, auto_index: bool) -> Result<Vec<Reference>> {
    maybe_ensure_index(auto_index)?;
    let conn = open_db()?;
    schema::initialize(&conn)?;
    Ok(crate::facade::references_with_meta_opts(&conn, name, None)?.results)
}

/// References with confidence metadata, capped at `limit` (`None` = default).
pub fn run_references_meta(
    name: &str,
    module: Option<&str>,
    limit: Option<usize>,
    auto_index: bool,
) -> Result<crate::graph::query_result::QueryResult<Reference>> {
    maybe_ensure_index(auto_index)?;
    let conn = open_db()?;
    schema::initialize(&conn)?;
    let qr = crate::facade::references_with_meta_opts(&conn, name, module)?;
    Ok(qr.truncated(crate::graph::query_result::resolve_limit(limit)))
}

/// Look up callers of `name` with import-aware precision when a unique
/// definition module can be determined; otherwise falls back to all sites.
/// Uncapped (see [`run_references`]).
pub fn run_callers(name: &str, auto_index: bool) -> Result<Vec<Reference>> {
    maybe_ensure_index(auto_index)?;
    let conn = open_db()?;
    schema::initialize(&conn)?;
    Ok(crate::facade::callers_with_meta_opts(&conn, name, None)?.results)
}

/// Callers with confidence metadata, capped at `limit` (`None` = default).
pub fn run_callers_meta(
    name: &str,
    module: Option<&str>,
    limit: Option<usize>,
    auto_index: bool,
) -> Result<crate::graph::query_result::QueryResult<Reference>> {
    maybe_ensure_index(auto_index)?;
    let conn = open_db()?;
    schema::initialize(&conn)?;
    let qr = crate::facade::callers_with_meta_opts(&conn, name, module)?;
    Ok(qr.truncated(crate::graph::query_result::resolve_limit(limit)))
}

/// Look up trait implementations by trait name.
pub fn run_implementations(trait_name: &str, auto_index: bool) -> Result<Vec<ImplRecord>> {
    Ok(run_implementations_meta(trait_name, None, auto_index)?.results)
}

/// Implementations with confidence metadata.
pub fn run_implementations_meta(
    trait_name: &str,
    module: Option<&str>,
    auto_index: bool,
) -> Result<crate::graph::query_result::QueryResult<ImplRecord>> {
    maybe_ensure_index(auto_index)?;
    let conn = open_db()?;
    schema::initialize(&conn)?;
    crate::facade::implementations_with_meta_opts(&conn, trait_name, module)
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

/// Look up modules that depend on `name` (module path, file, or symbol).
pub fn run_dependents(name: &str, auto_index: bool) -> Result<Vec<Dependency>> {
    Ok(run_dependents_meta(name, auto_index)?.results)
}

/// Dependents with confidence metadata.
pub fn run_dependents_meta(
    name: &str,
    auto_index: bool,
) -> Result<crate::graph::query_result::QueryResult<Dependency>> {
    maybe_ensure_index(auto_index)?;
    let conn = open_db()?;
    schema::initialize(&conn)?;
    crate::facade::dependents_with_meta(&conn, name)
}

/// Look up symbols transitively impacted by changing `name`.
/// Uncapped (see [`run_references`]).
pub fn run_impact(name: &str, auto_index: bool) -> Result<Vec<Symbol>> {
    maybe_ensure_index(auto_index)?;
    let conn = open_db()?;
    schema::initialize(&conn)?;
    Ok(crate::facade::impact_with_meta_opts(&conn, name, None)?.results)
}

/// Impact with confidence metadata, capped at `limit` (`None` = default).
pub fn run_impact_meta(
    name: &str,
    module: Option<&str>,
    limit: Option<usize>,
    auto_index: bool,
) -> Result<crate::graph::query_result::QueryResult<Symbol>> {
    maybe_ensure_index(auto_index)?;
    let conn = open_db()?;
    schema::initialize(&conn)?;
    let qr = crate::facade::impact_with_meta_opts(&conn, name, module)?;
    Ok(qr.truncated(crate::graph::query_result::resolve_limit(limit)))
}

/// Serve the JSON API on `127.0.0.1:{port}` using the on-disk index.
pub fn run_serve(port: u16, auto_index: bool) -> Result<()> {
    if auto_index {
        ensure_index(&query_root())?;
    } else {
        let conn = open_db()?;
        schema::initialize(&conn)?;
        drop(conn);
    }
    let addr = format!("127.0.0.1:{port}");
    api::serve(&addr, &db_path(), auto_index)
}

/// Marker file recording the background Insights server (`<port> [<pid>]`).
const INSIGHTS_PORT_FILE: &str = "insights.port";
/// Where the background Insights server's stderr goes.
const INSIGHTS_LOG_FILE: &str = "insights.log";
/// Env var suppressing the browser launch (`1`/`true`).
const NO_BROWSER_ENV: &str = "KEEL_NO_BROWSER";

/// Open the Insights dashboard, starting a background server when needed.
///
/// Reuses the server recorded in `./.keel/insights.port` when it still
/// answers `/health`, else adopts a hand-started `keel serve` on `preferred`
/// when it serves this project; otherwise binds `preferred` (a free port is
/// picked when it is busy, `0` means any free port), spawns a detached
/// `keel serve`, waits for `/health`, and opens the dashboard in the default
/// browser. `json` prints `{"url","port","pid","reused"}` instead of opening
/// a browser.
pub fn run_insights(preferred: u16, auto_index: bool, json: bool) -> Result<()> {
    let dir = Path::new(DB_DIR);
    let port_file = dir.join(INSIGHTS_PORT_FILE);
    if let Some((port, pid)) = read_insights_addr(&port_file) {
        if server_healthy(port) {
            // A healthy `/health` with a failing API means a stale server
            // (e.g. pre-dating the index schema): warn loudly instead of
            // opening a broken dashboard in silence. Never auto-killed: the
            // user may have started it by hand.
            // `--json` stays machine-clean: the warning is text-mode only.
            let api_error = if json { None } else { server_api_error(port) };
            if let Some(e) = api_error {
                eprintln!(
                    "WARNING: reused Insights server on port {port} reports an API error: {e}. It is probably an older Keel; restart it with: keel insights-stop (then run keel insights again)."
                );
            }
            return finish_insights(port, pid, true, json);
        }
    }
    std::fs::create_dir_all(dir).map_err(|source| KeelError::Io {
        path: dir.to_path_buf(),
        source,
    })?;
    // A `keel serve` the user started by hand is just as good: adopt it when
    // it serves this project instead of spawning a second server. (A server
    // for a *different* project is left alone; we spawn our own below.)
    if preferred != 0 {
        if let Some(root) = server_project_root(preferred) {
            if same_project(&root) {
                let _ = std::fs::write(&port_file, preferred.to_string());
                return finish_insights(preferred, None, true, json);
            }
        }
    }
    // Retry: the probed port can be stolen before the child binds it.
    let mut last_err = String::from("no attempt made");
    for _ in 0..3 {
        let port = pick_free_port(preferred)?;
        let mut child = match spawn_insights_server(port, auto_index, dir) {
            Ok(child) => child,
            Err(e) => {
                last_err = e.to_string();
                continue;
            }
        };
        if wait_for_health(port, std::time::Duration::from_secs(15)) {
            let pid = child.id();
            std::fs::write(&port_file, format!("{port} {pid}")).map_err(|source| {
                KeelError::Io {
                    path: port_file.clone(),
                    source,
                }
            })?;
            return finish_insights(port, Some(pid), false, json);
        }
        last_err = format!("server on port {port} never answered /health");
        let _ = child.kill();
    }
    Err(KeelError::Daemon(format!(
        "could not start Insights server ({last_err}); see {DB_DIR}/{INSIGHTS_LOG_FILE}"
    )))
}

/// Parse an insights marker file (`<port> [<pid>]`); `None` when missing/garbled.
fn read_insights_addr(path: &Path) -> Option<(u16, Option<u32>)> {
    let text = std::fs::read_to_string(path).ok()?;
    let mut parts = text.split_whitespace();
    let port = parts.next()?.parse::<u16>().ok()?;
    if port == 0 {
        return None;
    }
    let pid = parts.next().and_then(|p| p.parse::<u32>().ok());
    Some((port, pid))
}

/// `preferred` when it is free, else any free loopback port (`0` = any).
fn pick_free_port(preferred: u16) -> Result<u16> {
    use std::net::TcpListener;
    if preferred != 0
        && TcpListener::bind(format!("127.0.0.1:{preferred}"))
            .is_ok()
    {
        return Ok(preferred);
    }
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|source| KeelError::Io {
        path: PathBuf::from("127.0.0.1:0"),
        source,
    })?;
    listener
        .local_addr()
        .map(|addr| addr.port())
        .map_err(|source| KeelError::Io {
            path: PathBuf::from("127.0.0.1:0"),
            source,
        })
}

/// Spawn a detached `keel serve --port <port>`; stderr goes to the insights log.
fn spawn_insights_server(
    port: u16,
    auto_index: bool,
    dir: &Path,
) -> Result<std::process::Child> {
    use std::process::{Command, Stdio};
    let exe = std::env::current_exe().map_err(|source| KeelError::Io {
        path: PathBuf::from("keel"),
        source,
    })?;
    let log_path = dir.join(INSIGHTS_LOG_FILE);
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|source| KeelError::Io {
            path: log_path,
            source,
        })?;
    let mut cmd = Command::new(exe);
    cmd.arg("serve").arg("--port").arg(port.to_string());
    if !auto_index {
        cmd.arg("--no-auto-index");
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .spawn()
        .map_err(|source| KeelError::Io {
            path: PathBuf::from("keel serve"),
            source,
        })
}

/// Raw `GET <path>` against a loopback `port`: `(status, body)`.
///
/// `None` on any transport failure. Reads are bounded (1 MiB) so a foreign
/// endless stream on the port cannot hang the caller.
fn http_get(port: u16, path: &str) -> Option<(u16, String)> {
    use std::io::{Read, Write};
    use std::net::{SocketAddr, TcpStream};
    use std::time::Duration;
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(300)).ok()?;
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .ok()?;
    let req = format!("GET {path} HTTP/1.0\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).ok()?;
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => raw.extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
        if raw.len() > 1024 * 1024 {
            break;
        }
    }
    let text = String::from_utf8_lossy(&raw);
    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())?;
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
    Some((status, body))
}

/// True when a Keel server answers `/health` on `port` (200 + `"ok"` body).
fn server_healthy(port: u16) -> bool {
    matches!(http_get(port, "/health"), Some((200, body)) if body.contains("\"ok\""))
}

/// Server-side error from `GET /api/insights` (`Some` only when the payload
/// carries a non-empty `error`, e.g. a stale server that cannot read a newer
/// index schema). Transport/parse failures are `None` (no information).
fn server_api_error(port: u16) -> Option<String> {
    let (_, body) = http_get(port, "/api/insights")?;
    let value: serde_json::Value = serde_json::from_str(&body).ok()?;
    let error = value.get("error")?.as_str()?;
    if error.is_empty() {
        None
    } else {
        Some(error.to_string())
    }
}

/// Project root served by the Keel server on `port` (`None` when not Keel).
fn server_project_root(port: u16) -> Option<PathBuf> {
    let (status, body) = http_get(port, "/api/insights")?;
    if status != 200 {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(&body).ok()?;
    value
        .get("project")?
        .get("root")?
        .as_str()
        .map(PathBuf::from)
}

/// True when `root` is the current project (both sides canonicalized).
fn same_project(root: &Path) -> bool {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    match (cwd.canonicalize(), root.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// Poll `/health` until it answers or `timeout` elapses.
fn wait_for_health(port: u16, timeout: std::time::Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if server_healthy(port) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    server_healthy(port)
}

/// True when `KEEL_NO_BROWSER=1`/`true` (headless use, tests).
fn browser_suppressed() -> bool {
    std::env::var(NO_BROWSER_ENV)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Open `url` in the default browser (best-effort; skipped when suppressed).
fn open_browser(url: &str) -> Result<()> {
    if browser_suppressed() {
        return Ok(());
    }
    let (prog, extra): (&str, &[&str]) = if cfg!(target_os = "macos") {
        ("open", &[])
    } else if cfg!(target_os = "windows") {
        ("cmd", &["/C", "start", ""])
    } else {
        ("xdg-open", &[])
    };
    let mut cmd = std::process::Command::new(prog);
    cmd.args(extra).arg(url);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let status = cmd.status().map_err(|source| KeelError::Io {
        path: PathBuf::from(prog),
        source,
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(KeelError::Daemon(format!("{prog} exited with {status}")))
    }
}

/// Report the dashboard URL: JSON shape in `json` mode, else browser + hints.
fn finish_insights(port: u16, pid: Option<u32>, reused: bool, json: bool) -> Result<()> {
    let url = format!("http://127.0.0.1:{port}/insights");
    if json {
        println!(
            "{}",
            serde_json::json!({"url": url, "port": port, "pid": pid, "reused": reused})
        );
        return Ok(());
    }
    if reused {
        if pid.is_some() {
            println!("Reusing running Insights server on {url}");
        } else {
            println!("Reusing already-running Keel server on {url}");
        }
    } else {
        println!("Insights dashboard on {url}");
    }
    if let Err(e) = open_browser(&url) {
        eprintln!("Could not open a browser ({e}); open the URL above manually.");
    }
    if let Some(pid) = pid {
        if cfg!(target_os = "windows") {
            println!("Background server pid {pid}; stop it with: taskkill /PID {pid} /F");
        } else {
            println!("Background server pid {pid}; stop it with: kill {pid}");
        }
    }
    Ok(())
}

/// Stop this project's background Insights server.
pub fn run_insights_stop() -> Result<()> {
    run_insights_stop_at(Path::new("."))
}

/// Stop the Insights server recorded under `root`.
///
/// A hand-started `keel serve` we merely adopted (no pid on record) is left
/// running: the marker is forgotten and the command says so.
fn run_insights_stop_at(root: &Path) -> Result<()> {
    let port_file = root.join(DB_DIR).join(INSIGHTS_PORT_FILE);
    let Some((port, pid)) = read_insights_addr(&port_file) else {
        println!("No Insights server recorded for this project.");
        return Ok(());
    };
    if !server_healthy(port) {
        let _ = std::fs::remove_file(&port_file);
        println!("No Insights server running on port {port} (cleaned up stale marker).");
        return Ok(());
    }
    let Some(pid) = pid else {
        let _ = std::fs::remove_file(&port_file);
        println!(
            "Server on port {port} was started by hand (`keel serve`); left running, forgot the marker."
        );
        return Ok(());
    };
    // The marker pid may have been recycled since the server died: never
    // signal a live process we cannot attribute to Keel.
    if !crate::daemon::pid_is_keel(pid) {
        let _ = std::fs::remove_file(&port_file);
        println!(
            "Stale Insights marker (pid {pid} is not a Keel server); cleaned up. If a server still runs on port {port}, stop it by hand."
        );
        return Ok(());
    }
    terminate_process(pid)?;
    let start = std::time::Instant::now();
    while start.elapsed() < std::time::Duration::from_secs(3) {
        if !server_healthy(port) {
            let _ = std::fs::remove_file(&port_file);
            println!("Stopped Insights server on port {port} (pid {pid}).");
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    Err(KeelError::Daemon(format!(
        "signaled pid {pid} but port {port} still answers; stop it by hand"
    )))
}

/// SIGTERM (Unix) / force-kill (Windows) `pid`, refusing nonsense pids.
fn terminate_process(pid: u32) -> Result<()> {
    if pid == 0 || pid == u32::MAX || pid == std::process::id() {
        return Err(KeelError::Daemon(format!("refusing to signal pid {pid}")));
    }
    let pid_arg = pid.to_string();
    #[cfg(target_os = "windows")]
    let (prog, args): (&str, Vec<&str>) = ("taskkill", vec!["/PID", &pid_arg, "/F"]);
    #[cfg(not(target_os = "windows"))]
    let (prog, args): (&str, Vec<&str>) = ("kill", vec!["-TERM", &pid_arg]);
    let status = std::process::Command::new(prog)
        .args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(|source| KeelError::Io {
            path: PathBuf::from(prog),
            source,
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(KeelError::Daemon(format!("{prog} failed for pid {pid}")))
    }
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
            container: String::new(),
        };
        let reference = Reference {
            name: "create_order".into(),
            file: PathBuf::from("src/main.rs"),
            start_line: 3,
            start_col: 5,
            kind: ReferenceKind::Call,
            container: String::new(),
            qualifier: String::new(),
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
        let snippet = mcp_config_snippet(
            Path::new("/opt/keel/bin/keel"),
            Path::new("/home/u/proj"),
        );
        assert!(snippet.contains("mcpServers"), "{snippet}");
        assert!(snippet.contains("/opt/keel/bin/keel"), "{snippet}");
        assert!(snippet.contains("\"mcp\""), "{snippet}");
        // Project root pinned for spawn-cwd-independent resolution.
        assert!(snippet.contains("\"cwd\": \"/home/u/proj\""), "{snippet}");
        assert!(
            snippet.contains("\"KEEL_INDEX_DB\": \"/home/u/proj/.keel/index.db\""),
            "{snippet}"
        );
        assert!(
            snippet.contains(
                "claude mcp add keel -e KEEL_INDEX_DB=/home/u/proj/.keel/index.db -- /opt/keel/bin/keel mcp"
            ),
            "{snippet}"
        );
    }

    #[test]
    fn run_index_rejects_missing_and_file_paths_without_side_effects() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        // Debug render: the reason rides the error source (anyhow prints the
        // chain for real CLI runs; `Display` alone names only the path).
        let err = format!("{:?}", run_index(&missing).unwrap_err());
        assert!(err.contains("no such directory"), "got: {err}");
        assert!(!missing.exists(), "typo'd path must not be created");

        let file = dir.path().join("f.rs");
        std::fs::write(&file, "fn f() {}\n").unwrap();
        let err = format!("{:?}", run_index(&file).unwrap_err());
        assert!(err.contains("not a directory"), "got: {err}");
    }

    #[test]
    fn extra_miss_notes_skips_canonical_marker() {
        let notes = vec![
            "No matching symbols found.".to_string(),
            "No symbols matching `zzz`.".to_string(),
            "Did you mean `AuthService`?".to_string(),
        ];
        assert_eq!(extra_miss_notes(&notes), vec!["Did you mean `AuthService`?"]);
        assert!(extra_miss_notes(&[]).is_empty());
    }

    #[test]
    fn file_scope_note_detection_and_passthrough() {
        let notes = vec!["Also used at file scope in main.py (no enclosing symbol); see `references helper`."
            .to_string()];
        assert!(has_file_scope_note(&notes));
        assert!(!has_file_scope_note(&[]));
        assert!(!has_file_scope_note(&["No impacted symbols found.".to_string()]));
        // The bridge note is never filtered from human output.
        assert_eq!(extra_miss_notes(&notes).len(), 1);
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
    fn init_succeeds_without_daemon() {
        let _guard = ENV_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        isolate_daemon(&home.path().join("keel-home"));
        let proj = tempfile::tempdir().unwrap();
        std::fs::write(proj.path().join("main.py"), "X = 1\n").unwrap();

        run_init(proj.path()).unwrap();

        assert!(proj.path().join(".keel").join("index.db").exists());

        std::env::remove_var("KEEL_HOME");
        std::env::remove_var("KEEL_DAEMON_PORT");
    }

    #[test]
    fn pick_free_port_prefers_free_preferred() {
        use std::net::TcpListener;
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        assert_eq!(pick_free_port(port).unwrap(), port);
    }

    #[test]
    fn pick_free_port_skips_busy_port() {
        use std::net::TcpListener;
        let busy = TcpListener::bind("127.0.0.1:0").unwrap();
        let busy_port = busy.local_addr().unwrap().port();
        // Ephemeral ports can be grabbed between pick and re-bind by parallel
        // tests; retry the pick-then-bind pair a few times before failing.
        let mut bound = false;
        for _ in 0..5 {
            let picked = pick_free_port(busy_port).unwrap();
            assert_ne!(picked, busy_port);
            if TcpListener::bind(format!("127.0.0.1:{picked}")).is_ok() {
                bound = true;
                break;
            }
        }
        assert!(bound, "fallback port was never bindable");
        drop(busy);
    }

    #[test]
    fn server_healthy_rejects_dead_port() {
        use std::net::TcpListener;
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        assert!(!server_healthy(port));
    }

    #[test]
    fn server_healthy_accepts_keel_health() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut req = [0u8; 512];
            let _ = stream.read(&mut req);
            let body = r#"{"status":"ok"}"#;
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        });
        assert!(server_healthy(port));
        handle.join().unwrap();
    }

    #[test]
    fn server_project_root_reads_payload_root() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut req = [0u8; 512];
            let _ = stream.read(&mut req);
            let body = r#"{"project":{"root":"/tmp/some-project"}}"#;
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        });
        assert_eq!(
            server_project_root(port),
            Some(PathBuf::from("/tmp/some-project"))
        );
        handle.join().unwrap();
    }

    #[test]
    fn server_project_root_rejects_dead_port() {
        use std::net::TcpListener;
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        assert_eq!(server_project_root(port), None);
    }

    #[test]
    fn same_project_matches_cwd_only() {
        let cwd = std::env::current_dir().unwrap();
        assert!(same_project(&cwd));
        let elsewhere = tempfile::tempdir().unwrap();
        assert!(!same_project(elsewhere.path()));
    }

    #[test]
    fn open_browser_suppressed_by_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var("KEEL_NO_BROWSER", "1");
        let result = open_browser("http://example.invalid/");
        std::env::remove_var("KEEL_NO_BROWSER");
        assert!(result.is_ok());
    }

    #[test]
    fn read_insights_addr_parses_port_and_pid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("insights.port");
        std::fs::write(&path, "7645 1234").unwrap();
        assert_eq!(read_insights_addr(&path), Some((7645, Some(1234))));
        std::fs::write(&path, "7645").unwrap();
        assert_eq!(read_insights_addr(&path), Some((7645, None)));
        std::fs::write(&path, "garbage").unwrap();
        assert_eq!(read_insights_addr(&path), None);
    }

    /// Fake Keel server answering health/insights, then exiting after `max` hits.
    fn spawn_capped_fake_server(max: usize) -> (u16, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            for _ in 0..max {
                let (mut stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                let mut req = [0u8; 512];
                let _ = stream.read(&mut req);
                let body = if req.starts_with(b"GET /health") {
                    r#"{"status":"ok"}"#
                } else {
                    r#"{"project":{"root":"/tmp/x"}}"#
                };
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            }
        });
        (port, handle)
    }

    #[test]
    fn insights_stop_without_marker_is_ok() {
        let root = tempfile::tempdir().unwrap();
        run_insights_stop_at(root.path()).unwrap();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn insights_stop_spares_recycled_non_keel_pid() {
        // Healthy port + marker pid that now belongs to another program:
        // the marker is cleaned but the foreign process must survive.
        let mut foreign = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(DB_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let (port, handle) = spawn_capped_fake_server(1);
        let marker = dir.join(INSIGHTS_PORT_FILE);
        std::fs::write(&marker, format!("{port} {}", foreign.id())).unwrap();
        run_insights_stop_at(root.path()).unwrap();
        assert!(!marker.exists(), "stale marker must be cleaned");
        assert!(
            foreign.try_wait().unwrap().is_none(),
            "foreign process must survive insights-stop"
        );
        foreign.kill().ok();
        let _ = foreign.wait();
        handle.join().unwrap();
    }

    #[test]
    fn insights_status_tracks_live_marker() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(insights_status(root.path()), InsightsStatus::Stopped);
        let dir = root.path().join(DB_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        // Dead port with a marker still reads stopped.
        let (dead, dead_handle) = spawn_capped_fake_server(0);
        dead_handle.join().unwrap();
        let marker = dir.join(INSIGHTS_PORT_FILE);
        std::fs::write(&marker, dead.to_string()).unwrap();
        assert_eq!(insights_status(root.path()), InsightsStatus::Stopped);
        // Live marker reads running with its port.
        let (port, handle) = spawn_capped_fake_server(2);
        std::fs::write(&marker, port.to_string()).unwrap();
        assert_eq!(
            insights_status(root.path()),
            InsightsStatus::Running {
                port,
                api_error: None
            }
        );
        handle.join().unwrap();
    }

    #[test]
    fn server_api_error_surfaces_error_payloads_only() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        for (body, expected) in [
            (
                r#"{"error":"database schema version 4 is newer than supported version 3"}"#,
                Some("database schema version 4 is newer than supported version 3"),
            ),
            (r#"{"project":{"root":"/tmp/x"}}"#, None),
            (r#"{"error":""}"#, None),
            ("not json", None),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let handle = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut req = [0u8; 512];
                let _ = stream.read(&mut req);
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                );
            });
            assert_eq!(server_api_error(port).as_deref(), expected);
            handle.join().unwrap();
        }
        // Dead port: no information, not an error.
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let dead = probe.local_addr().unwrap().port();
        drop(probe);
        assert_eq!(server_api_error(dead), None);
    }

    #[test]
    fn doctor_checks_json_lists_all_checks() {
        let root = tempfile::tempdir().unwrap();
        let text = doctor_checks_json(root.path()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let names: Vec<&str> = v["checks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["version", "daemon", "project", "mcp", "index"]);
        for c in v["checks"].as_array().unwrap() {
            assert!(c["ok"].is_boolean(), "ok must be bool: {c}");
            assert!(c["detail"].is_string(), "detail must be string: {c}");
        }
    }

    #[test]
    fn status_json_reports_stopped_daemon() {
        let _guard = ENV_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        isolate_daemon(&home.path().join("keel-home"));
        let root = tempfile::tempdir().unwrap();

        let text = status_json(root.path()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["daemon"]["daemon"], "stopped");
        assert!(v["daemon"]["hint"].is_string());
        assert!(v["daemon"]["index"].as_str().unwrap().ends_with("index.db"));
        assert_eq!(v["insights"]["state"], "stopped");

        std::env::remove_var("KEEL_HOME");
        std::env::remove_var("KEEL_DAEMON_PORT");
    }

    #[test]
    fn insights_stop_cleans_stale_marker() {
        use std::net::TcpListener;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(DB_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let dead = probe.local_addr().unwrap().port();
        drop(probe);
        let marker = dir.join(INSIGHTS_PORT_FILE);
        std::fs::write(&marker, format!("{dead} 999999")).unwrap();

        run_insights_stop_at(root.path()).unwrap();

        assert!(!marker.exists());
    }

    #[test]
    fn insights_stop_forgets_adopted_server() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(DB_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        let (port, handle) = spawn_capped_fake_server(1);
        let marker = dir.join(INSIGHTS_PORT_FILE);
        std::fs::write(&marker, port.to_string()).unwrap();

        run_insights_stop_at(root.path()).unwrap();

        assert!(!marker.exists());
        handle.join().unwrap();
    }

    #[test]
    fn terminate_process_refuses_nonsense_pids() {
        assert!(terminate_process(0).is_err());
        assert!(terminate_process(u32::MAX).is_err());
        assert!(terminate_process(std::process::id()).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn insights_stop_kills_recorded_pid() {
        use std::os::unix::process::CommandExt;
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join(DB_DIR);
        std::fs::create_dir_all(&dir).unwrap();
        // Two answers (initial check + one poll), then the port goes dead.
        let (port, handle) = spawn_capped_fake_server(2);
        // argv[0] carries "keel" so pid-identity verification attributes
        // this stand-in (a bare `sleep` would be spared as recycled).
        let mut sleeper = std::process::Command::new("sleep")
            .arg0("keel-fake-server")
            .arg("30")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let marker = dir.join(INSIGHTS_PORT_FILE);
        std::fs::write(&marker, format!("{port} {}", sleeper.id())).unwrap();

        run_insights_stop_at(root.path()).unwrap();

        assert!(!marker.exists());
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(2) {
            if sleeper.try_wait().unwrap().is_some() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        assert!(sleeper.try_wait().unwrap().is_some());
        handle.join().unwrap();
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

    #[test]
    fn preview_cache_reads_truncates_and_skips_stale_rows() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let long = "x".repeat(PREVIEW_MAX_CHARS + 10);
        std::fs::write(
            root.join("a.rs"),
            format!("\u{FEFF}fn first() {{}}\r\n{long}\nlast\n"),
        )
        .unwrap();
        let mut cache = PreviewCache::new(root.to_path_buf());
        // BOM stripped, CR stripped by line splitting.
        assert_eq!(
            cache.line(Path::new("a.rs"), 1).as_deref(),
            Some("fn first() {}")
        );
        // Long lines truncate with an ellipsis marker.
        let got = cache.line(Path::new("a.rs"), 2).unwrap();
        assert_eq!(got.chars().count(), PREVIEW_MAX_CHARS + 1);
        assert!(got.ends_with('…'));
        // Missing files, line 0, and out-of-range lines preview as nothing.
        assert_eq!(cache.line(Path::new("gone.rs"), 1), None);
        assert_eq!(cache.line(Path::new("a.rs"), 0), None);
        assert_eq!(cache.line(Path::new("a.rs"), 99), None);
    }
}

