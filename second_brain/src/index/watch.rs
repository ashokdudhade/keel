//! Filesystem watching: re-index a repository when `.rs` files change.

use crate::error::{Result, SecondBrainError};
use crate::index::{self, IndexStats};
use notify::{Event, RecursiveMode, Watcher};
use rusqlite::Connection;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Default debounce window before a pending re-index runs.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(200);

/// Watch `root` for changes and re-index into `conn` until interrupted.
///
/// File events are coalesced with a ~200ms debounce. Each debounce flush calls
/// [`index::index_repository`]. Runs until the watcher channel closes or an
/// error is returned.
pub fn watch_repository(root: &Path, conn: &mut Connection) -> Result<()> {
    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res| {
        let _ = tx.send(res);
    })
    .map_err(|e| SecondBrainError::Watch(format!("setup failed: {e}")))?;

    watcher
        .watch(root, RecursiveMode::Recursive)
        .map_err(|e| SecondBrainError::Watch(format!("failed: {e}")))?;

    // Initial index so the DB is warm before the first change.
    let stats = index::index_repository(root, conn)?;
    eprintln!(
        "watch: initial index — indexed={}, skipped={}, removed={}",
        stats.indexed, stats.skipped, stats.removed
    );

    let mut pending = false;
    let mut deadline = Instant::now();

    loop {
        let timeout = if pending {
            deadline.saturating_duration_since(Instant::now())
        } else {
            Duration::from_secs(3600)
        };

        match rx.recv_timeout(timeout) {
            Ok(Ok(event)) => {
                if is_relevant_event(&event) {
                    pending = true;
                    deadline = Instant::now() + DEFAULT_DEBOUNCE;
                }
            }
            Ok(Err(e)) => {
                return Err(SecondBrainError::Watch(format!("event error: {e}")));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if pending {
                    pending = false;
                    let stats = reindex_on_change(root, conn)?;
                    eprintln!(
                        "watch: re-indexed — indexed={}, skipped={}, removed={}",
                        stats.indexed, stats.skipped, stats.removed
                    );
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // Keep the watcher alive until the loop ends.
    drop(watcher);
    Ok(())
}

/// Run one incremental re-index pass. Extracted so tests can invoke the handler
/// without waiting on the filesystem watcher debounce timer.
pub fn reindex_on_change(root: &Path, conn: &mut Connection) -> Result<IndexStats> {
    index::index_repository(root, conn)
}

/// Return true when an event may have affected indexed Rust sources.
fn is_relevant_event(event: &Event) -> bool {
    use notify::EventKind;
    match event.kind {
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_) => event
            .paths
            .iter()
            .any(|p| p.extension().and_then(|s| s.to_str()) == Some("rs")),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn reindex_on_change_handler_is_incremental() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/lib.rs"), "fn first() {}\n").unwrap();

        let mut conn = Connection::open_in_memory().unwrap();
        let initial = reindex_on_change(root, &mut conn).unwrap();
        assert_eq!(initial.indexed, 1);

        // Unchanged: handler should skip.
        let again = reindex_on_change(root, &mut conn).unwrap();
        assert_eq!(again.indexed, 0);
        assert_eq!(again.skipped, 1);

        // Modify then reindex via the same handler the watcher calls.
        fs::write(root.join("src/lib.rs"), "fn first() {}\nfn second() {}\n").unwrap();
        let changed = reindex_on_change(root, &mut conn).unwrap();
        assert_eq!(changed.indexed, 1);
        assert_eq!(changed.skipped, 0);
    }
}
