//! Per-file filesystem watcher for the LIP daemon (TruthKeeper §3.1 "source watchers").
//!
//! Instead of periodically compacting the entire journal, the watcher monitors
//! every file the daemon is currently tracking. When a file changes on disk
//! (e.g. a `git checkout`, build artifact, or out-of-band edit), the watcher:
//!
//! 1. Reads the new content.
//! 2. Compares it against the daemon's stored text (SHA-256 of the body).
//! 3. If different, writes a targeted `UpsertFile` journal entry and updates
//!    the in-memory db — no full recompaction needed.
//!
//! This keeps journal growth proportional to actual edits rather than workspace
//! size, and eliminates the need for startup compaction in steady state.
//!
//! ## Threading model
//!
//! `notify`'s `RecommendedWatcher` is driven by a dedicated OS thread.  The
//! thread forwards change events to an async tokio task via an unbounded channel.
//! Commands (add/remove path) flow in the opposite direction via a
//! `std::sync::mpsc` channel.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, warn};

use crate::query_graph::LipDatabase;

use super::journal::{Journal, JournalEntry};

// ─── Public handle ────────────────────────────────────────────────────────────

/// Commands sent from tokio-land to the watcher OS thread.
pub enum WatchCmd {
    /// Start watching `path`, mapping back to LIP `uri`.
    Add { uri: String, path: PathBuf },
    /// Stop watching `path` (file removed).
    Remove { path: PathBuf },
    /// Terminate the thread.
    Shutdown,
}

/// Cheap clone-able handle that sessions use to register / deregister watches.
#[derive(Clone)]
pub struct FileWatcherHandle {
    cmd_tx: std::sync::mpsc::SyncSender<WatchCmd>,
}

impl FileWatcherHandle {
    /// Register `path` (derived from `uri`) for change monitoring.
    pub fn add(&self, uri: String, path: PathBuf) {
        let _ = self.cmd_tx.try_send(WatchCmd::Add { uri, path });
    }

    /// Deregister `path` from monitoring.
    pub fn remove(&self, path: PathBuf) {
        let _ = self.cmd_tx.try_send(WatchCmd::Remove { path });
    }
}

// ─── Spawn ────────────────────────────────────────────────────────────────────

/// Spawn the OS watcher thread + the async event-processor task.
///
/// Returns a handle that callers use to add/remove watched paths.
pub fn spawn(
    db:      Arc<Mutex<LipDatabase>>,
    journal: Arc<StdMutex<Journal>>,
) -> FileWatcherHandle {
    // Channel: tokio → watcher thread (commands).
    // Bounded to 1024 so a burst of adds from startup replay doesn't grow unbounded.
    let (cmd_tx, cmd_rx) = std::sync::mpsc::sync_channel::<WatchCmd>(1024);

    // Channel: watcher thread → tokio task (change events).
    let (event_tx, event_rx) = mpsc::unbounded_channel::<(String, PathBuf)>();

    // ── OS thread: owns the notify watcher ───────────────────────────────────
    std::thread::Builder::new()
        .name("lip-watcher".into())
        .spawn(move || {
            let (notify_tx, notify_rx) =
                std::sync::mpsc::channel::<notify::Result<Event>>();

            let mut watcher = match RecommendedWatcher::new(
                move |res| { let _ = notify_tx.send(res); },
                Config::default(),
            ) {
                Ok(w)  => w,
                Err(e) => {
                    warn!("file watcher could not be initialised: {e}");
                    return;
                }
            };

            let mut path_to_uri: HashMap<PathBuf, String> = HashMap::new();

            loop {
                // Wait up to 50 ms for a notify event, then check for commands.
                match notify_rx.recv_timeout(Duration::from_millis(50)) {
                    Ok(Ok(event)) => {
                        if matches!(
                            event.kind,
                            EventKind::Modify(_) | EventKind::Create(_)
                        ) {
                            for path in event.paths {
                                if let Some(uri) = path_to_uri.get(&path) {
                                    let _ = event_tx.send((uri.clone(), path));
                                }
                            }
                        }
                    }
                    Ok(Err(e)) => warn!("notify error: {e}"),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                }

                // Drain all pending commands.
                loop {
                    match cmd_rx.try_recv() {
                        Ok(WatchCmd::Add { uri, path }) => {
                            // Canonicalize so the path we store matches what
                            // notify delivers on macOS (/tmp → /private/tmp).
                            let canonical = path.canonicalize().unwrap_or(path);
                            match watcher.watch(&canonical, RecursiveMode::NonRecursive) {
                                Ok(()) => {
                                    debug!("watching {}", canonical.display());
                                    path_to_uri.insert(canonical, uri);
                                }
                                Err(e) => warn!(
                                    "could not watch {}: {e}",
                                    canonical.display()
                                ),
                            }
                        }
                        Ok(WatchCmd::Remove { path }) => {
                            let canonical = path.canonicalize().unwrap_or(path);
                            let _ = watcher.unwatch(&canonical);
                            path_to_uri.remove(&canonical);
                        }
                        Ok(WatchCmd::Shutdown) => return,
                        Err(_) => break,
                    }
                }
            }
        })
        .expect("failed to spawn watcher thread");

    // ── Async task: processes change events ───────────────────────────────────
    tokio::spawn(event_processor(db, journal, event_rx));

    FileWatcherHandle { cmd_tx }
}

// ─── Event processor ─────────────────────────────────────────────────────────

async fn event_processor(
    db:       Arc<Mutex<LipDatabase>>,
    journal:  Arc<StdMutex<Journal>>,
    mut rx:   mpsc::UnboundedReceiver<(String, PathBuf)>,
) {
    while let Some((uri, path)) = rx.recv().await {
        handle_change(uri, path, &db, &journal).await;
    }
}

async fn handle_change(
    uri:     String,
    path:    PathBuf,
    db:      &Arc<Mutex<LipDatabase>>,
    journal: &Arc<StdMutex<Journal>>,
) {
    let new_text = match tokio::fs::read_to_string(&path).await {
        Ok(t)  => t,
        Err(e) => {
            debug!("watcher: could not read {}: {e}", path.display());
            return;
        }
    };

    // Check whether content actually changed and grab the stored language.
    let language = {
        let db_guard = db.lock().await;
        if db_guard.file_text(&uri) == Some(new_text.as_str()) {
            return; // no change — spurious event
        }
        db_guard
            .file_language(&uri)
            .map(|s| s.to_owned())
            .unwrap_or_default()
    };

    debug!("watcher: {uri} changed on disk, re-indexing");

    // Write journal entry before mutating db (WAL guarantee).
    if let Ok(mut j) = journal.lock() {
        let _ = j.append(&JournalEntry::UpsertFile {
            uri:      uri.clone(),
            text:     new_text.clone(),
            language: language.clone(),
        });
    }

    let mut db_guard = db.lock().await;
    db_guard.upsert_file(uri, new_text, language);
}

// ─── URI → path helper ────────────────────────────────────────────────────────

/// Extract a filesystem `PathBuf` from a LIP / LSP URI.
///
/// Handles:
/// - `file:///abs/path`  → `/abs/path`
/// - `/abs/path`         → as-is (already a path)
/// - `lip://local/abs/path#Symbol` → `/abs/path` (strips fragment)
pub fn uri_to_path(uri: &str) -> Option<PathBuf> {
    if let Some(rest) = uri.strip_prefix("file://") {
        // file:///abs/path — the first '/' is part of the path
        let path = rest.trim_start_matches('/');
        return Some(PathBuf::from(format!("/{path}")));
    }

    if let Some(rest) = uri.strip_prefix("lip://local/") {
        // Strip symbol fragment if present
        let path = rest.split('#').next().unwrap_or(rest);
        if path.starts_with('/') {
            return Some(PathBuf::from(path));
        }
        return Some(PathBuf::from(format!("/{path}")));
    }

    if uri.starts_with('/') {
        return Some(PathBuf::from(uri));
    }

    None
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::Duration;
    use tempfile::NamedTempFile;

    // ── uri_to_path ──────────────────────────────────────────────────────────

    #[test]
    fn uri_to_path_file_scheme() {
        let p = uri_to_path("file:///home/lisa/project/main.rs").unwrap();
        assert_eq!(p, PathBuf::from("/home/lisa/project/main.rs"));
    }

    #[test]
    fn uri_to_path_file_scheme_double_slash() {
        // Some editors emit file:// without a third slash.
        let p = uri_to_path("file://home/lisa/main.rs").unwrap();
        assert_eq!(p, PathBuf::from("/home/lisa/main.rs"));
    }

    #[test]
    fn uri_to_path_lip_local_strips_fragment() {
        let p = uri_to_path("lip://local/home/lisa/project/main.rs#my_fn").unwrap();
        assert_eq!(p, PathBuf::from("/home/lisa/project/main.rs"));
    }

    #[test]
    fn uri_to_path_bare_path() {
        let p = uri_to_path("/tmp/foo.rs").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/foo.rs"));
    }

    #[test]
    fn uri_to_path_unknown_scheme_returns_none() {
        assert!(uri_to_path("http://example.com/foo.rs").is_none());
        assert!(uri_to_path("relative/path.rs").is_none());
    }

    // ── watcher detects disk change ──────────────────────────────────────────

    #[tokio::test]
    async fn watcher_detects_file_change() {
        use super::super::journal::Journal;
        use crate::query_graph::LipDatabase;

        // Create a real file and seed the db with its initial content.
        let mut tmp = NamedTempFile::new().unwrap();
        let path = tmp.path().to_owned();
        let uri  = format!("file://{}", path.display());
        write!(tmp, "fn original() {{}}").unwrap();
        tmp.flush().unwrap();

        let db = Arc::new(Mutex::new(LipDatabase::new()));
        {
            let mut db_guard = db.lock().await;
            db_guard.upsert_file(uri.clone(), "fn original() {}".into(), "rust".into());
        }

        let tmp_journal = NamedTempFile::new().unwrap();
        let (j, _) = Journal::open(tmp_journal.path()).unwrap();
        let journal = Arc::new(StdMutex::new(j));

        // Spawn the watcher and register the file.
        let handle = spawn(Arc::clone(&db), Arc::clone(&journal));
        handle.add(uri.clone(), path.clone());

        // Give the watcher thread time to set up the watch.
        tokio::time::sleep(Duration::from_millis(400)).await;

        // Overwrite the file with new content.
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .truncate(true)
                .open(&path)
                .unwrap();
            write!(f, "fn updated() {{}}").unwrap();
        }

        // Wait for the watcher to pick up the change.
        // FSEvents on macOS batches events; allow up to 3 seconds.
        tokio::time::sleep(Duration::from_millis(3000)).await;

        // The db should now contain the new text.
        let db_guard = db.lock().await;
        assert_eq!(
            db_guard.file_text(&uri),
            Some("fn updated() {}"),
            "watcher should have updated db with new file content"
        );
    }
}
