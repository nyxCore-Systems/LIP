use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tracing::{error, info};

use crate::query_graph::LipDatabase;

use super::session::Session;

/// The LIP daemon — accepts Unix socket connections and dispatches sessions.
pub struct LipDaemon {
    socket_path: PathBuf,
    db:          Arc<Mutex<LipDatabase>>,
}

impl LipDaemon {
    pub fn new(socket_path: impl AsRef<Path>) -> Self {
        Self {
            socket_path: socket_path.as_ref().to_owned(),
            db:          Arc::new(Mutex::new(LipDatabase::new())),
        }
    }

    /// Run the accept loop. Blocks until the process is killed.
    pub async fn run(self) -> anyhow::Result<()> {
        if self.socket_path.exists() {
            std::fs::remove_file(&self.socket_path)?;
        }
        let listener = UnixListener::bind(&self.socket_path)?;
        info!("LIP daemon listening on {}", self.socket_path.display());

        loop {
            let (stream, _) = listener.accept().await?;
            let session = Arc::new(Session::new(self.db.clone()));
            tokio::spawn(async move {
                if let Err(e) = session.run(stream).await {
                    error!("session error: {e}");
                }
            });
        }
    }
}
