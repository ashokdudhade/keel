//! Global Keel daemon: machine-level control plane for per-project watchers.
//!
//! Started via `brew services start keel` (or `keel daemon`). Projects register
//! with `keel start` so the daemon indexes and watches that tree into
//! `<project>/.keel/index.db`.

use crate::error::{Result, KeelError};
use crate::index;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};

/// Default loopback port for the global daemon control API.
pub const DEFAULT_DAEMON_PORT: u16 = 7646;

const DB_DIR: &str = ".keel";
const DB_FILE: &str = "index.db";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProjectEntry {
    path: String,
    pid: u32,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RegistryFile {
    projects: Vec<ProjectEntry>,
}

struct DaemonState {
    projects: HashMap<String, Child>,
}

/// Resolve `KEEL_HOME` (default `~/.keel`).
pub fn keel_home() -> PathBuf {
    if let Some(p) = std::env::var_os("KEEL_HOME") {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".keel")
}

fn daemon_dir() -> PathBuf {
    keel_home().join("daemon")
}

fn registry_path() -> PathBuf {
    daemon_dir().join("projects.json")
}

fn daemon_pid_path() -> PathBuf {
    daemon_dir().join("daemon.pid")
}

fn daemon_port_path() -> PathBuf {
    daemon_dir().join("daemon.port")
}

/// Port the running daemon advertises (or default).
pub fn discover_daemon_port() -> u16 {
    if let Ok(text) = fs::read_to_string(daemon_port_path()) {
        if let Ok(p) = text.trim().parse() {
            return p;
        }
    }
    std::env::var("KEEL_DAEMON_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_DAEMON_PORT)
}

fn ensure_daemon_dir() -> Result<()> {
    fs::create_dir_all(daemon_dir()).map_err(|source| KeelError::Io {
        path: daemon_dir(),
        source,
    })
}

fn load_registry() -> RegistryFile {
    fs::read_to_string(registry_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Absolute roots currently recorded in the daemon registry (`projects.json`).
///
/// Used by MCP index resolution when `KEEL_INDEX_DB` is unset.
pub fn registered_project_roots() -> Vec<PathBuf> {
    load_registry()
        .projects
        .into_iter()
        .map(|e| PathBuf::from(e.path))
        .filter(|p| !p.as_os_str().is_empty())
        .collect()
}

fn save_registry(reg: &RegistryFile) -> Result<()> {
    ensure_daemon_dir()?;
    let text = serde_json::to_string_pretty(reg).map_err(|e| KeelError::Daemon(e.to_string()))?;
    fs::write(registry_path(), text).map_err(|source| KeelError::Io {
        path: registry_path(),
        source,
    })
}

fn open_project_db(root: &Path) -> Result<Connection> {
    let dir = root.join(DB_DIR);
    fs::create_dir_all(&dir).map_err(|source| KeelError::Io {
        path: dir.clone(),
        source,
    })?;
    let db = dir.join(DB_FILE);
    let conn = Connection::open(&db)?;
    crate::db::configure_connection(&conn)?;
    Ok(conn)
}

/// Pids outside this range wrap or fail inside `kill` (e.g. `u32::MAX` becomes
/// pid `-1`, meaning "every process") and must never be signaled.
fn valid_pid(pid: u32) -> bool {
    pid > 0 && pid <= i32::MAX as u32
}

fn process_alive(pid: u32) -> bool {
    if !valid_pid(pid) {
        return false;
    }
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// True when `pid` looks like a Keel process.
///
/// Guards stale pidfiles/markers against PID reuse: a recycled pid may be
/// alive but belong to an unrelated program, which must never be signaled.
/// Linux checks `/proc/<pid>/cmdline`; dead/unreadable means "not ours".
/// Other platforms cannot verify cheaply and keep existing behavior.
pub(crate) fn pid_is_keel(pid: u32) -> bool {
    if !valid_pid(pid) {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        match std::fs::read(format!("/proc/{pid}/cmdline")) {
            Ok(bytes) => String::from_utf8_lossy(&bytes).contains("keel"),
            Err(_) => false,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        true
    }
}

fn signal_term(pid: u32) -> Result<()> {
    if !valid_pid(pid) {
        return Err(KeelError::Daemon(format!("refusing to signal invalid pid {pid}")));
    }
    let status = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|source| KeelError::Io {
            path: PathBuf::from("kill"),
            source,
        })?;
    if !status.success() {
        return Err(KeelError::Daemon(format!("kill -TERM {pid} failed")));
    }
    Ok(())
}

/// Run the global daemon until interrupted (brew services / foreground).
pub fn run_daemon(port: u16) -> Result<()> {
    ensure_daemon_dir()?;
    if daemon_pid_path().exists() {
        if let Ok(pid) = fs::read_to_string(daemon_pid_path()) {
            if let Ok(pid) = pid.trim().parse::<u32>() {
                // A live non-Keel pid is a stale pidfile after PID reuse:
                // fall through and overwrite it instead of refusing to start.
                if process_alive(pid) && pid_is_keel(pid) && pid != std::process::id() {
                    return Err(KeelError::Daemon(format!(
                        "keel daemon already running (pid {pid})"
                    )));
                }
            }
        }
    }

    fs::write(daemon_pid_path(), format!("{}\n", std::process::id())).map_err(|source| {
        KeelError::Io {
            path: daemon_pid_path(),
            source,
        }
    })?;
    fs::write(daemon_port_path(), format!("{port}\n")).map_err(|source| KeelError::Io {
        path: daemon_port_path(),
        source,
    })?;

    let state = Arc::new(Mutex::new(DaemonState {
        projects: HashMap::new(),
    }));

    // Restore previously registered projects (best-effort).
    for entry in load_registry().projects {
        let path = PathBuf::from(&entry.path);
        if path.is_dir() {
            if let Err(e) = start_project_watch(&state, &path) {
                eprintln!("daemon: failed to restore {}: {e}", path.display());
            }
        }
    }

    let addr = format!("127.0.0.1:{port}");
    eprintln!("keel daemon listening on http://{addr}");
    let server = Server::http(&addr).map_err(|e| KeelError::Daemon(e.to_string()))?;

    for request in server.incoming_requests() {
        handle_daemon_request(request, &state);
    }

    let _ = fs::remove_file(daemon_pid_path());
    Ok(())
}

fn handle_daemon_request(mut request: Request, state: &Arc<Mutex<DaemonState>>) {
    let method = request.method().clone();
    let url = request.url().to_string();
    let mut body_buf = String::new();
    if method == Method::Post {
        let _ = request.as_reader().read_to_string(&mut body_buf);
    }
    let (status, body) = match dispatch(&method, &url, &body_buf, state) {
        Ok(v) => v,
        Err(e) => (
            StatusCode(500),
            json!({ "error": e.to_string() }).to_string(),
        ),
    };
    let header = Header::from_bytes("Content-Type", "application/json").unwrap();
    let len = body.len();
    let response = Response::new(
        status,
        vec![header],
        std::io::Cursor::new(body.into_bytes()),
        Some(len),
        None,
    );
    let _ = request.respond(response);
}

fn dispatch(
    method: &Method,
    url: &str,
    body_buf: &str,
    state: &Arc<Mutex<DaemonState>>,
) -> Result<(StatusCode, String)> {
    let path = url.split('?').next().unwrap_or(url);

    if *method == Method::Get && path == "/health" {
        return Ok((
            StatusCode(200),
            json!({ "status": "ok", "daemon": true }).to_string(),
        ));
    }

    if *method == Method::Get && path == "/status" {
        let mut projects = Vec::new();
        {
            let mut guard = state.lock().map_err(|e| KeelError::Daemon(e.to_string()))?;
            guard.projects.retain(|_, child| matches!(child.try_wait(), Ok(None)));
            for (path, child) in &guard.projects {
                projects.push(json!({
                    "path": path,
                    "pid": child.id(),
                }));
            }
        }
        persist_state(state)?;
        return Ok((
            StatusCode(200),
            json!({
                "daemon": true,
                "pid": std::process::id(),
                "projects": projects,
            })
            .to_string(),
        ));
    }

    if *method == Method::Post && path == "/watch" {
        let body: serde_json::Value =
            serde_json::from_str(body_buf).map_err(|e| KeelError::Daemon(e.to_string()))?;
        let path = body
            .get("path")
            .and_then(|p| p.as_str())
            .ok_or_else(|| KeelError::Daemon("missing path".into()))?;
        let abs = fs::canonicalize(path).map_err(|source| KeelError::Io {
            path: PathBuf::from(path),
            source,
        })?;
        let pid = start_project_watch(state, &abs)?;
        persist_state(state)?;
        return Ok((
            StatusCode(200),
            json!({
                "ok": true,
                "path": abs.display().to_string(),
                "pid": pid,
            })
            .to_string(),
        ));
    }

    if *method == Method::Delete && path == "/watch" {
        let path = url
            .split('?')
            .nth(1)
            .unwrap_or("")
            .split('&')
            .find_map(|pair| {
                let mut it = pair.splitn(2, '=');
                match (it.next(), it.next()) {
                    (Some("path"), Some(v)) => Some(percent_decode(v)),
                    _ => None,
                }
            })
            .ok_or_else(|| KeelError::Daemon("missing path query".into()))?;
        let abs = fs::canonicalize(&path).unwrap_or_else(|_| PathBuf::from(&path));
        let key = abs.display().to_string();
        stop_project_watch(state, &key)?;
        persist_state(state)?;
        return Ok((
            StatusCode(200),
            json!({ "ok": true, "path": key }).to_string(),
        ));
    }

    Ok((StatusCode(404), json!({ "error": "not found" }).to_string()))
}

fn persist_state(state: &Arc<Mutex<DaemonState>>) -> Result<()> {
    let guard = state.lock().map_err(|e| KeelError::Daemon(e.to_string()))?;
    let reg = RegistryFile {
        projects: guard
            .projects
            .iter()
            .map(|(path, child)| ProjectEntry {
                path: path.clone(),
                pid: child.id(),
            })
            .collect(),
    };
    drop(guard);
    save_registry(&reg)
}

fn start_project_watch(state: &Arc<Mutex<DaemonState>>, abs_root: &Path) -> Result<u32> {
    let key = abs_root.display().to_string();
    {
        let mut guard = state.lock().map_err(|e| KeelError::Daemon(e.to_string()))?;
        if let Some(child) = guard.projects.get_mut(&key) {
            if matches!(child.try_wait(), Ok(None)) {
                return Ok(child.id());
            }
        }
    }

    // Project-level initial index into <project>/.keel/index.db
    let mut conn = open_project_db(abs_root)?;
    let stats = index::index_repository(abs_root, &mut conn)?;
    drop(conn);
    eprintln!(
        "daemon: indexed {} — indexed={}, skipped={}, removed={}, errors={}, syntax_errors={}",
        abs_root.display(),
        stats.indexed,
        stats.skipped,
        stats.removed,
        stats.errors,
        stats.syntax_errors
    );

    let exe = std::env::current_exe().map_err(|source| KeelError::Io {
        path: PathBuf::from("keel"),
        source,
    })?;
    let log_path = abs_root.join(DB_DIR).join("watch.log");
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|source| KeelError::Io {
            path: log_path.clone(),
            source,
        })?;
    let log_err = log.try_clone().map_err(|source| KeelError::Io {
        path: log_path,
        source,
    })?;

    let child = Command::new(&exe)
        .arg("watch")
        .arg(abs_root)
        .current_dir(abs_root)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err))
        .env("KEEL_SERVICE", "1")
        .spawn()
        .map_err(|source| KeelError::Io {
            path: exe,
            source,
        })?;
    let pid = child.id();

    let mut guard = state.lock().map_err(|e| KeelError::Daemon(e.to_string()))?;
    guard.projects.insert(key, child);
    Ok(pid)
}

fn stop_project_watch(state: &Arc<Mutex<DaemonState>>, key: &str) -> Result<()> {
    let mut guard = state.lock().map_err(|e| KeelError::Daemon(e.to_string()))?;
    if let Some(mut child) = guard.projects.remove(key) {
        let pid = child.id();
        let _ = signal_term(pid);
        let _ = child.wait();
    }
    Ok(())
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2])) {
                out.push((hi << 4) | lo);
                i += 3;
                continue;
            }
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn percent_encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len() * 2);
    for b in path.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn http_json(method: &str, path: &str, body: Option<&str>) -> Result<(u16, String)> {
    let port = discover_daemon_port();
    let addr = format!("127.0.0.1:{port}");
    let mut stream = TcpStream::connect(&addr).map_err(|source| KeelError::Io {
        path: PathBuf::from(&addr),
        source,
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .ok();
    let body = body.unwrap_or("");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .write_all(req.as_bytes())
        .map_err(|source| KeelError::Io {
            path: PathBuf::from(&addr),
            source,
        })?;
    let mut buf = Vec::new();
    stream
        .read_to_end(&mut buf)
        .map_err(|source| KeelError::Io {
            path: PathBuf::from(&addr),
            source,
        })?;
    let text = String::from_utf8_lossy(&buf);
    let status = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(500);
    let body = text
        .split("\r\n\r\n")
        .nth(1)
        .unwrap_or("")
        .to_string();
    Ok((status, body))
}

/// How to start the global daemon on this platform.
///
/// Homebrew is macOS-only; elsewhere the foreground `keel daemon` is the path.
pub fn daemon_start_hint() -> &'static str {
    if cfg!(target_os = "macos") {
        "brew services start keel"
    } else {
        "keel daemon"
    }
}

/// True when the global daemon control port responds.
pub fn daemon_reachable() -> bool {
    http_json("GET", "/health", None)
        .map(|(code, _)| code == 200)
        .unwrap_or(false)
}

/// Register a project with the global daemon (index + watch).
pub fn client_start_project(path: &Path) -> Result<()> {
    if !daemon_reachable() {
        let hint = daemon_start_hint();
        let mut msg = format!("keel daemon is not running. Start it with: {hint}");
        if cfg!(target_os = "macos") {
            msg.push_str("\n(or: keel daemon)");
        }
        return Err(KeelError::Daemon(msg));
    }
    let abs = fs::canonicalize(path).map_err(|source| KeelError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let payload = json!({ "path": abs.display().to_string() }).to_string();
    let (code, body) = http_json("POST", "/watch", Some(&payload))?;
    if code != 200 {
        return Err(KeelError::Daemon(format!(
            "daemon rejected start ({code}): {body}"
        )));
    }
    let v: serde_json::Value =
        serde_json::from_str(&body).unwrap_or_else(|_| json!({ "raw": body }));
    println!(
        "Watching {} (pid {}) via global keel daemon",
        abs.display(),
        v.get("pid").and_then(|p| p.as_u64()).unwrap_or(0)
    );
    Ok(())
}

/// Stop the global daemon and its project watchers.
pub fn client_stop_daemon() -> Result<()> {
    let mut pid = fs::read_to_string(daemon_pid_path())
        .ok()
        .and_then(|t| t.trim().parse::<u32>().ok());
    // A live non-Keel pid is PID reuse after an unclean death: drop the
    // stale pidfile instead of counting (or killing) someone else's process.
    // A truly running daemon is still caught by reachability below.
    if pid.is_some_and(|p| process_alive(p) && !pid_is_keel(p)) {
        let _ = fs::remove_file(daemon_pid_path());
        pid = None;
    }
    let was_running = pid.map(process_alive).unwrap_or(false) || daemon_reachable();
    if !was_running {
        println!("keel daemon is not running.");
        return Ok(());
    }
    // Stop watchers first: otherwise they orphan and double-index after the
    // next daemon start spawns fresh ones. Registry pids are verified for
    // the same reuse reason (a recorded watcher may long be dead).
    for entry in load_registry().projects {
        if process_alive(entry.pid) && pid_is_keel(entry.pid) {
            let _ = signal_term(entry.pid);
        }
    }
    if let Some(pid) = pid {
        if process_alive(pid) {
            signal_term(pid)?;
            for _ in 0..20 {
                if !process_alive(pid) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            if process_alive(pid) {
                return Err(KeelError::Daemon(format!(
                    "keel daemon (pid {pid}) did not stop"
                )));
            }
        }
    }
    let _ = fs::remove_file(daemon_pid_path());
    if daemon_reachable() {
        return Err(KeelError::Daemon(
            "keel daemon port still responds; stop it manually (brew services stop keel)".into(),
        ));
    }
    println!("keel daemon stopped.");
    Ok(())
}

/// One health check result for `keel doctor`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DoctorCheck {
    /// Short check name (`daemon`, `project`, `index`, `mcp`).
    pub name: &'static str,
    /// False when the user should act; `detail` says how.
    pub ok: bool,
    /// One-line status, including the fix when `!ok`.
    pub detail: String,
}

/// Diagnose daemon, project registration, index, and MCP health for `project`.
pub fn doctor_checks(project: &Path) -> Vec<DoctorCheck> {
    let mut out = vec![DoctorCheck {
        name: "version",
        ok: true,
        detail: env!("CARGO_PKG_VERSION").to_string(),
    }];

    let abs = fs::canonicalize(project).unwrap_or_else(|_| project.to_path_buf());
    let key = abs.display().to_string();
    // Captured once so follow-up advice never suggests daemon-only commands
    // (`keel start`) when the daemon is down — `keel init` works daemon-less.
    let daemon_up = daemon_reachable();
    if daemon_up {
        out.push(DoctorCheck {
            name: "daemon",
            ok: true,
            detail: "running".into(),
        });
    } else {
        out.push(DoctorCheck {
            name: "daemon",
            ok: false,
            detail: format!("stopped (start it with: {})", daemon_start_hint()),
        });
    }

    let entry = load_registry().projects.into_iter().find(|e| e.path == key);
    match entry {
        Some(e) if process_alive(e.pid) => out.push(DoctorCheck {
            name: "project",
            ok: true,
            detail: format!("watching {} (pid {})", key, e.pid),
        }),
        Some(e) => out.push(DoctorCheck {
            name: "project",
            ok: false,
            detail: if daemon_up {
                format!(
                    "registered but watcher pid {} is dead (run: keel start {})",
                    e.pid,
                    project.display()
                )
            } else {
                format!(
                    "registered but watcher pid {} is dead (run: keel daemon, then keel start {})",
                    e.pid,
                    project.display()
                )
            },
        }),
        None => out.push(DoctorCheck {
            name: "project",
            ok: false,
            detail: if daemon_up {
                format!("not registered (run: keel start {})", project.display())
            } else {
                format!(
                    "not registered (run: keel daemon, then keel start {})",
                    project.display()
                )
            },
        }),
    }

    out.push(mcp_check());

    let db = project.join(DB_DIR).join(DB_FILE);
    if !db.is_file() {
        // `keel start` needs the daemon; `keel init` indexes one-shot now.
        let setup = if daemon_up { "start" } else { "init" };
        out.push(DoctorCheck {
            name: "index",
            ok: false,
            detail: format!(
                "missing {} (run: keel {setup} {})",
                db.display(),
                project.display()
            ),
        });
        return out;
    }
    match open_doctor_db(&db) {
        Ok(conn) => {
            // SQLite opens lazily, so a corrupt file reaches this branch
            // and its failed queries degrade to "0 files, format v0" —
            // validate the header first so corruption isn't misreported
            // as a stale-but-rebuildable format.
            if !is_sqlite_db(&db) {
                out.push(DoctorCheck {
                    name: "index",
                    ok: false,
                    detail: format!(
                        "unreadable (not a SQLite database; delete and re-index: rm -rf {})",
                        project.join(DB_DIR).display(),
                    ),
                });
                return out;
            }
            let files: i64 = conn
                .query_row("SELECT COUNT(*) FROM files", [], |row| row.get(0))
                .unwrap_or(0);
            let format = crate::db::schema::index_format_version(&conn);
            if format != crate::db::schema::INDEX_FORMAT_VERSION {
                out.push(DoctorCheck {
                    name: "index",
                    ok: false,
                    detail: format!(
                        "{files} file(s) but format v{format} is stale (run any query to rebuild, or: rm -rf {})",
                        project.join(DB_DIR).display(),
                    ),
                });
            } else {
                out.push(DoctorCheck {
                    name: "index",
                    ok: files > 0,
                    detail: if files > 0 {
                        format!("{files} file(s), format current")
                    } else {
                        "empty (run: keel index . or keel start)".into()
                    },
                });
            }
        }
        Err(e) => out.push(DoctorCheck {
            name: "index",
            ok: false,
            detail: format!("unreadable ({e})"),
        }),
    }
    out
}

/// Verify the MCP server answers `tools/list` over a stdio loopback.
///
/// Spawns this binary as `keel mcp`, runs initialize + tools/list with a 10s
/// timeout, and reports the tool count. `tools/list` is answered before the
/// DB opens, so this works with no index present.
fn mcp_check() -> DoctorCheck {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("keel"));
    match mcp_tools_via_stdio(&exe) {
        Ok(tools) if !tools.is_empty() => DoctorCheck {
            name: "mcp",
            ok: true,
            detail: format!("{} tools via stdio loopback", tools.len()),
        },
        Ok(_) => DoctorCheck {
            name: "mcp",
            ok: false,
            detail: "tools/list returned no tools (reinstall keel)".into(),
        },
        Err(e) => DoctorCheck {
            name: "mcp",
            ok: false,
            detail: format!("stdio loopback failed ({e})"),
        },
    }
}

/// Ask `exe mcp` for its tool list over stdio (10s timeout).
fn mcp_tools_via_stdio(exe: &Path) -> Result<Vec<String>> {
    use std::io::{BufRead, BufReader};
    let mut child = Command::new(exe)
        .arg("mcp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|source| KeelError::Io {
            path: exe.to_path_buf(),
            source,
        })?;
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| KeelError::Mcp("mcp stdio loopback has no stdin".into()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| KeelError::Mcp("mcp stdio loopback has no stdout".into()))?;
    let exe_owned = exe.to_path_buf();
    let io_err = move |source: std::io::Error| KeelError::Io {
        path: exe_owned.clone(),
        source,
    };
    // The conversation runs on a worker thread so the 10s timeout below can
    // fire even when the child never answers.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = (|| -> Result<Vec<String>> {
            let init = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {"name": "keel-doctor", "version": "0"},
                },
            });
            writeln!(stdin, "{init}").map_err(&io_err)?;
            writeln!(stdin, "{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}}")
                .map_err(&io_err)?;
            writeln!(
                stdin,
                "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\",\"params\":{{}}}}"
            )
            .map_err(&io_err)?;
            stdin.flush().map_err(&io_err)?;
            drop(stdin);
            for line in BufReader::new(stdout).lines() {
                let line = line.map_err(&io_err)?;
                if line.trim().is_empty() {
                    continue;
                }
                let v: serde_json::Value =
                    serde_json::from_str(&line).map_err(|e| KeelError::Daemon(e.to_string()))?;
                if v.get("id") == Some(&json!(2)) {
                    let tools = v
                        .pointer("/result/tools")
                        .and_then(|t| t.as_array())
                        .ok_or_else(|| {
                            KeelError::Mcp("tools/list response has no tools".into())
                        })?;
                    return Ok(tools
                        .iter()
                        .filter_map(|t| t.get("name")?.as_str().map(str::to_owned))
                        .collect());
                }
            }
            Err(KeelError::Mcp(
                "mcp stdio loopback got no tools/list response".into(),
            ))
        })();
        let _ = tx.send(result);
    });
    let result = rx.recv_timeout(Duration::from_secs(10));
    let _ = child.kill();
    let _ = child.wait();
    match result {
        Ok(r) => r,
        Err(_) => Err(KeelError::Mcp(
            "mcp stdio loopback timed out after 10s".into(),
        )),
    }
}

/// Open an index for read-only inspection without creating or migrating it.
fn open_doctor_db(db: &Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(conn)
}

/// True when `path` starts with the SQLite magic header (first 16 bytes).
fn is_sqlite_db(path: &Path) -> bool {
    use std::io::Read;
    let mut magic = [0u8; 16];
    std::fs::File::open(path)
        .and_then(|mut f| f.read_exact(&mut magic).map(|_| magic))
        .is_ok_and(|m| m == *b"SQLite format 3\0")
}

/// Unregister the current project from the global daemon.
pub fn client_stop_project(path: &Path) -> Result<()> {
    if !daemon_reachable() {
        println!("keel daemon is not running.");
        return Ok(());
    }
    let abs = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let encoded = percent_encode_path(&abs.display().to_string());
    let (code, body) = http_json("DELETE", &format!("/watch?path={encoded}"), None)?;
    if code != 200 {
        return Err(KeelError::Daemon(format!(
            "daemon rejected stop ({code}): {body}"
        )));
    }
    println!("Stopped watching {}.", abs.display());
    Ok(())
}

/// Print global daemon + current project watch status.
pub fn client_status(path: &Path) -> Result<()> {
    let s = daemon_status(path)?;
    match s.daemon.as_str() {
        "stopped" => {
            println!("daemon:\tstopped");
            println!("hint:\t{}", s.hint.unwrap_or_default());
            println!("index:\t{}", s.index.unwrap_or_default());
        }
        "running" => {
            println!("daemon:\trunning");
            println!("daemon_pid:\t{}", s.daemon_pid.unwrap_or(0));
            println!("projects:\t{}", s.projects.len());
            for p in &s.projects {
                println!("  - {} (pid {})", p.path, p.pid);
            }
            println!(
                "this_project:\t{}",
                s.this_project.as_deref().unwrap_or("not watching")
            );
            println!("index:\t{}", s.index.unwrap_or_default());
        }
        other => {
            println!("daemon:\t{other}");
            if let Some(body) = s.error_body {
                println!("{body}");
            }
        }
    }
    Ok(())
}

/// A project watched by the global daemon.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct WatchedProject {
    /// Registered project root.
    pub path: String,
    /// Watcher process id.
    pub pid: u64,
}

/// Machine-readable daemon status (also backs `client_status` printing).
///
/// `daemon` is `stopped`, `running`, or `error (CODE)`; only the fields
/// relevant to that state are populated.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DaemonStatus {
    /// Daemon state: `stopped`, `running`, or `error (CODE)`.
    pub daemon: String,
    /// Start hint (stopped only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Local index path (stopped and running).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index: Option<String>,
    /// Daemon pid (running only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub daemon_pid: Option<u64>,
    /// Watched projects (running only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub projects: Vec<WatchedProject>,
    /// `watching` or `not watching` for the queried project (running only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub this_project: Option<String>,
    /// Raw error body (error only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_body: Option<String>,
}

/// Query the global daemon for machine-readable status.
pub fn daemon_status(path: &Path) -> Result<DaemonStatus> {
    let index = path.join(DB_DIR).join(DB_FILE);
    if !daemon_reachable() {
        return Ok(DaemonStatus {
            daemon: "stopped".into(),
            hint: Some(daemon_start_hint().to_string()),
            index: Some(index.display().to_string()),
            daemon_pid: None,
            projects: Vec::new(),
            this_project: None,
            error_body: None,
        });
    }
    let (code, body) = http_json("GET", "/status", None)?;
    if code != 200 {
        return Ok(DaemonStatus {
            daemon: format!("error ({code})"),
            hint: None,
            index: None,
            daemon_pid: None,
            projects: Vec::new(),
            this_project: None,
            error_body: Some(body),
        });
    }
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| KeelError::Daemon(e.to_string()))?;
    let abs = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let key = abs.display().to_string();
    let mut projects = Vec::new();
    let mut watching = false;
    if let Some(arr) = v.get("projects").and_then(|p| p.as_array()) {
        for p in arr {
            let ppath = p.get("path").and_then(|x| x.as_str()).unwrap_or("");
            let pid = p.get("pid").and_then(|x| x.as_u64()).unwrap_or(0);
            projects.push(WatchedProject {
                path: ppath.to_string(),
                pid,
            });
            if ppath == key {
                watching = true;
            }
        }
    }
    Ok(DaemonStatus {
        daemon: "running".into(),
        hint: None,
        index: Some(index.display().to_string()),
        daemon_pid: Some(v.get("pid").and_then(|p| p.as_u64()).unwrap_or(0)),
        projects,
        this_project: Some(if watching { "watching" } else { "not watching" }.into()),
        error_body: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_pids_are_never_alive_nor_signaled() {
        // u32::MAX wraps to -1 ("every process") inside kill(2): it must read
        // as dead, and signaling it must be refused outright.
        assert!(!process_alive(u32::MAX));
        assert!(!process_alive(0));
        assert!(signal_term(u32::MAX).is_err());
        assert!(signal_term(0).is_err());
    }

    #[test]
    fn pid_identity_accepts_self_and_rejects_recycled_pids() {
        // The test binary's own path contains "keel" (Linux); other
        // platforms cannot verify and accept.
        assert!(pid_is_keel(std::process::id()));
        assert!(!pid_is_keel(0));
        assert!(!pid_is_keel(u32::MAX));
        #[cfg(target_os = "linux")]
        {
            let mut child = std::process::Command::new("sleep")
                .arg("30")
                .spawn()
                .expect("spawn sleep");
            assert!(!pid_is_keel(child.id()));
            child.kill().ok();
            let _ = child.wait();
        }
    }

    #[test]
    fn start_hint_is_platform_appropriate() {
        if cfg!(target_os = "macos") {
            assert_eq!(daemon_start_hint(), "brew services start keel");
        } else {
            // Homebrew-only advice strands Linux/WSL users; the foreground
            // daemon is the documented path there.
            assert_eq!(daemon_start_hint(), "keel daemon");
        }
    }

    #[test]
    fn sqlite_magic_distinguishes_databases_from_garbage() {
        let dir = tempfile::tempdir().unwrap();
        let garbage = dir.path().join("garbage.db");
        std::fs::write(&garbage, b"CORRUPT!".repeat(100)).unwrap();
        assert!(!is_sqlite_db(&garbage));
        let empty = dir.path().join("empty.db");
        std::fs::write(&empty, b"").unwrap();
        assert!(!is_sqlite_db(&empty));
        assert!(!is_sqlite_db(&dir.path().join("missing.db")));
        let real = dir.path().join("real.db");
        let conn = Connection::open(&real).unwrap();
        conn.execute("CREATE TABLE t (x INTEGER)", [])
            .unwrap();
        drop(conn);
        assert!(is_sqlite_db(&real));
    }

    #[test]
    #[cfg(unix)]
    fn mcp_loopback_reads_tool_list() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-mcp.sh");
        std::fs::write(
            &script,
            r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"id":1'*) echo '{"jsonrpc":"2.0","id":1,"result":{"capabilities":{}}}' ;;
    *'"id":2'*) echo '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"definition"},{"name":"outline"}]}}' ;;
  esac
done
"#,
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let tools = mcp_tools_via_stdio(&script).unwrap();
        assert_eq!(tools, vec!["definition".to_string(), "outline".to_string()]);
    }

    #[test]
    #[cfg(unix)]
    fn mcp_loopback_reports_dead_server() {
        assert!(mcp_tools_via_stdio(Path::new("/bin/true")).is_err());
        assert!(mcp_tools_via_stdio(Path::new("/nonexistent-keel-mcp")).is_err());
    }
}
