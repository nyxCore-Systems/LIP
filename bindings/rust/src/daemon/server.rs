use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::net::UnixListener;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info};

use crate::query_graph::LipDatabase;

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
            ));
            tokio::spawn(async move {
                if let Err(e) = session.run(stream).await {
                    error!("session error: {e}");
                }
            });
        }
    }
}
