//! Auto-index on query and global daemon + project watch lifecycle tests.

use std::fs;
use std::net::TcpListener;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

fn keel_bin() -> &'static str {
    env!("CARGO_BIN_EXE_keel")
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_daemon(port: u16, home: &Path) {
    for _ in 0..50 {
        let out = Command::new(keel_bin())
            .env("KEEL_HOME", home)
            .env("KEEL_DAEMON_PORT", port.to_string())
            .args(["status"])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        if stdout.contains("daemon:\trunning") {
            return;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("daemon did not become ready on port {port}");
}

fn spawn_daemon(port: u16, home: &Path) -> Child {
    fs::create_dir_all(home).unwrap();
    Command::new(keel_bin())
        .env("KEEL_HOME", home)
        .env("KEEL_DAEMON_PORT", port.to_string())
        .args(["daemon", "--port", &port.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn daemon")
}

#[test]
fn definition_auto_indexes_missing_db() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "pub struct AutoIndexed;\nfn helper() {}\n",
    )
    .unwrap();

    assert!(!root.join(".keel/index.db").exists());

    let out = Command::new(keel_bin())
        .current_dir(root)
        .args(["definition", "AutoIndexed"])
        .output()
        .unwrap();
    assert!(out.status.success(), "stderr={}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("AutoIndexed"),
        "expected definition in stdout: {stdout}"
    );
    assert!(root.join(".keel/index.db").exists());
}

#[test]
fn no_auto_index_skips_ensure() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub struct Skipped;\n").unwrap();

    let out = Command::new(keel_bin())
        .current_dir(root)
        .args(["--no-auto-index", "definition", "Skipped"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stdout.contains("Skipped"),
        "should not find symbol without index: {stdout}"
    );
    assert!(
        stderr.contains("No definition found") || stdout.is_empty(),
        "stderr={stderr}"
    );
}

#[test]
fn second_query_skips_unchanged_files_quietly() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub struct Quiet;\n").unwrap();

    let first = Command::new(keel_bin())
        .current_dir(root)
        .args(["definition", "Quiet"])
        .output()
        .unwrap();
    assert!(first.status.success());
    let first_err = String::from_utf8_lossy(&first.stderr);
    assert!(
        first_err.contains("auto-indexed") || first.status.success(),
        "first ensure may log: {first_err}"
    );

    let second = Command::new(keel_bin())
        .current_dir(root)
        .args(["definition", "Quiet"])
        .output()
        .unwrap();
    assert!(second.status.success());
    let second_err = String::from_utf8_lossy(&second.stderr);
    assert!(
        !second_err.contains("auto-indexed"),
        "unchanged tree should not log auto-index: {second_err}"
    );
}

#[test]
fn start_without_daemon_errors() {
    let home = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub struct X;\n").unwrap();

    let port = free_port();
    let start = Command::new(keel_bin())
        .current_dir(root)
        .env("KEEL_HOME", home.path())
        .env("KEEL_DAEMON_PORT", port.to_string())
        .args(["start", "."])
        .output()
        .unwrap();
    assert!(!start.status.success());
    let err = format!(
        "{}{}",
        String::from_utf8_lossy(&start.stdout),
        String::from_utf8_lossy(&start.stderr)
    );
    assert!(
        err.contains("daemon is not running") || err.contains("brew services start keel"),
        "expected daemon hint: {err}"
    );
}

#[test]
fn daemon_start_status_stop_lifecycle() {
    let home = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub struct Serviced;\n").unwrap();

    let port = free_port();
    let mut daemon = spawn_daemon(port, home.path());
    wait_daemon(port, home.path());

    let start = Command::new(keel_bin())
        .current_dir(root)
        .env("KEEL_HOME", home.path())
        .env("KEEL_DAEMON_PORT", port.to_string())
        .args(["start", "."])
        .output()
        .unwrap();
    assert!(
        start.status.success(),
        "start failed: {}",
        String::from_utf8_lossy(&start.stderr)
    );
    assert!(root.join(".keel/index.db").exists());

    thread::sleep(Duration::from_millis(200));

    let status = Command::new(keel_bin())
        .current_dir(root)
        .env("KEEL_HOME", home.path())
        .env("KEEL_DAEMON_PORT", port.to_string())
        .arg("status")
        .output()
        .unwrap();
    assert!(status.status.success());
    let status_out = String::from_utf8_lossy(&status.stdout);
    assert!(
        status_out.contains("daemon:\trunning"),
        "expected daemon running: {status_out}"
    );
    assert!(
        status_out.contains("this_project:\twatching"),
        "expected watching: {status_out}"
    );

    let stop = Command::new(keel_bin())
        .current_dir(root)
        .env("KEEL_HOME", home.path())
        .env("KEEL_DAEMON_PORT", port.to_string())
        .arg("stop")
        .output()
        .unwrap();
    assert!(
        stop.status.success(),
        "stop failed: {}",
        String::from_utf8_lossy(&stop.stderr)
    );

    let status2 = Command::new(keel_bin())
        .current_dir(root)
        .env("KEEL_HOME", home.path())
        .env("KEEL_DAEMON_PORT", port.to_string())
        .arg("status")
        .output()
        .unwrap();
    let status2_out = String::from_utf8_lossy(&status2.stdout);
    assert!(
        status2_out.contains("this_project:\tnot watching"),
        "expected not watching: {status2_out}"
    );

    let _ = daemon.kill();
    let _ = daemon.wait();
}

#[test]
fn index_root_from_db_path() {
    use keel::cli::commands::index_root_from_db;
    assert_eq!(
        index_root_from_db(Path::new("/proj/.keel/index.db")),
        Path::new("/proj")
    );
    assert_eq!(
        index_root_from_db(Path::new(".keel/index.db")),
        Path::new(".")
    );
}
