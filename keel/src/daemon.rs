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
    let text = serde_json::to_string_pretty(reg).map_err(|e| KeelError::Watch(e.to_string()))?;
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

fn process_alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn signal_term(pid: u32) -> Result<()> {
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
        return Err(KeelError::Watch(format!("kill -TERM {pid} failed")));
    }
    Ok(())
}

/// Run the global daemon until interrupted (brew services / foreground).
pub fn run_daemon(port: u16) -> Result<()> {
    ensure_daemon_dir()?;
    if daemon_pid_path().exists() {
        if let Ok(pid) = fs::read_to_string(daemon_pid_path()) {
            if let Ok(pid) = pid.trim().parse::<u32>() {
                if process_alive(pid) && pid != std::process::id() {
                    return Err(KeelError::Watch(format!(
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
    let server = Server::http(&addr).map_err(|e| KeelError::Watch(e.to_string()))?;

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
            let mut guard = state.lock().map_err(|e| KeelError::Watch(e.to_string()))?;
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
            serde_json::from_str(body_buf).map_err(|e| KeelError::Watch(e.to_string()))?;
        let path = body
            .get("path")
            .and_then(|p| p.as_str())
            .ok_or_else(|| KeelError::Watch("missing path".into()))?;
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
            .ok_or_else(|| KeelError::Watch("missing path query".into()))?;
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
    let guard = state.lock().map_err(|e| KeelError::Watch(e.to_string()))?;
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
        let mut guard = state.lock().map_err(|e| KeelError::Watch(e.to_string()))?;
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
        "daemon: indexed {} — indexed={}, skipped={}, removed={}, errors={}",
        abs_root.display(),
        stats.indexed,
        stats.skipped,
        stats.removed,
        stats.errors
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

    let mut guard = state.lock().map_err(|e| KeelError::Watch(e.to_string()))?;
    guard.projects.insert(key, child);
    Ok(pid)
}

fn stop_project_watch(state: &Arc<Mutex<DaemonState>>, key: &str) -> Result<()> {
    let mut guard = state.lock().map_err(|e| KeelError::Watch(e.to_string()))?;
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

/// True when the global daemon control port responds.
pub fn daemon_reachable() -> bool {
    http_json("GET", "/health", None)
        .map(|(code, _)| code == 200)
        .unwrap_or(false)
}

/// Register a project with the global daemon (index + watch).
pub fn client_start_project(path: &Path) -> Result<()> {
    if !daemon_reachable() {
        return Err(KeelError::Watch(
            "keel daemon is not running. Start it with: brew services start keel\n\
             (or: keel daemon)"
                .into(),
        ));
    }
    let abs = fs::canonicalize(path).map_err(|source| KeelError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let payload = json!({ "path": abs.display().to_string() }).to_string();
    let (code, body) = http_json("POST", "/watch", Some(&payload))?;
    if code != 200 {
        return Err(KeelError::Watch(format!(
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
        return Err(KeelError::Watch(format!(
            "daemon rejected stop ({code}): {body}"
        )));
    }
    println!("Stopped watching {}.", abs.display());
    Ok(())
}

/// Print global daemon + current project watch status.
pub fn client_status(path: &Path) -> Result<()> {
    let index = path.join(DB_DIR).join(DB_FILE);
    if !daemon_reachable() {
        println!("daemon:\tstopped");
        println!("hint:\tbrew services start keel");
        println!("index:\t{}", index.display());
        return Ok(());
    }
    let (code, body) = http_json("GET", "/status", None)?;
    if code != 200 {
        println!("daemon:\terror ({code})");
        println!("{body}");
        return Ok(());
    }
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| KeelError::Watch(e.to_string()))?;
    println!("daemon:\trunning");
    println!(
        "daemon_pid:\t{}",
        v.get("pid").and_then(|p| p.as_u64()).unwrap_or(0)
    );
    let abs = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let key = abs.display().to_string();
    let mut watching = false;
    if let Some(arr) = v.get("projects").and_then(|p| p.as_array()) {
        println!("projects:\t{}", arr.len());
        for p in arr {
            let ppath = p.get("path").and_then(|x| x.as_str()).unwrap_or("");
            let pid = p.get("pid").and_then(|x| x.as_u64()).unwrap_or(0);
            println!("  - {ppath} (pid {pid})");
            if ppath == key {
                watching = true;
            }
        }
    }
    println!(
        "this_project:\t{}",
        if watching { "watching" } else { "not watching" }
    );
    println!("index:\t{}", index.display());
    Ok(())
}
