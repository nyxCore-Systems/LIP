use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use tokio::net::UnixListener;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, warn};

use crate::query_graph::LipDatabase;

use super::journal::{self, Journal, COMPACT_THRESHOLD as COMPACT_THR};
use super::session::Session;
use super::tier2_manager::{Tier2Manager, VerificationJob, CHANNEL_CAPACITY};

/// The LIP daemon — accepts Unix socket connections and dispatches sessions.
pub struct LipDaemon {
    socket_path: PathBuf,
    db:          Arc<Mutex<LipDatabase>>,
    tier2_tx:    mpsc::Sender<VerificationJob>,
    tier2_rx:    Option<mpsc::Receiver<VerificationJob>>,
}

impl LipDaemon {
    pub fn new(socket_path: impl AsRef<Path>) -> Self {
        let (tier2_tx, tier2_rx) = mpsc::channel(CHANNEL_CAPACITY);
        Self {
            socket_path: socket_path.as_ref().to_owned(),
            db:          Arc::new(Mutex::new(LipDatabase::new())),
            tier2_tx,
            tier2_rx:    Some(tier2_rx),
        }
    }

    /// Run the accept loop. Blocks until the process is killed.
    pub async fn run(mut self) -> anyhow::Result<()> {
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path)?;
        }

        // Open the write-ahead journal. The path mirrors the socket with a
        // `.journal` extension so they live in the same directory.
        let journal_path = {
            let name = self.socket_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            self.socket_path.with_file_name(format!("{name}.journal"))
        };
        let (_, entries) = Journal::open(&journal_path)?;
        // Replay persisted entries into the db before accepting connections.
        {
            let mut db = self.db.lock().await;
            journal::replay(&entries, &mut db);
            if entries.len() >= COMPACT_THR {
                match journal::compact(&journal_path, &db) {
                    Ok(n) => info!(
                        "compacted journal: {} entries → {} snapshot entries ({})",
                        entries.len(), n, journal_path.display()
                    ),
                    Err(e) => warn!("journal compaction failed: {e}"),
                }
            } else if !entries.is_empty() {
                info!("replayed {} journal entries from {}", entries.len(), journal_path.display());
            }
        }
        // Re-open for appending (post-compaction file or original if below threshold).
        let raw_journal = Journal::open_append(&journal_path)?;
        let shared_journal = Arc::new(StdMutex::new(raw_journal));

        let listener = UnixListener::bind(&self.socket_path)?;
        info!("LIP daemon listening on {}", self.socket_path.display());

        // Spawn the Tier 2 background manager. It is a separate task so Tier 2
        // verification never blocks session response latency.
        let rx = self.tier2_rx.take().expect("tier2_rx consumed exactly once");
        let manager = Tier2Manager::new(self.db.clone(), rx);
        tokio::spawn(async move { manager.run().await });

        loop {
            let (stream, _) = listener.accept().await?;
            let session = Arc::new(Session::new(
                self.db.clone(),
                Some(self.tier2_tx.clone()),
                Some(Arc::clone(&shared_journal)),
            ));
            tokio::spawn(async move {
                if let Err(e) = session.run(stream).await {
                    error!("session error: {e}");
                }
            });
        }
    }
}
