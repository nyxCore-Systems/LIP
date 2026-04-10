//! Background Tier 2 verification manager.
//!
//! [`Tier2Manager`] runs as a dedicated tokio task alongside the accept loop.
//! Sessions push [`VerificationJob`]s onto a bounded channel; the manager
//! processes them one at a time using a [`RustAnalyzerBackend`].
//!
//! The channel is bounded (capacity 64) so that a slow rust-analyzer does not
//! cause unbounded memory growth. When the channel is full, [`try_send`] in
//! the session drops the job silently — Tier 1 results remain available.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info, warn};

use crate::indexer::tier2::rust_analyzer::RustAnalyzerBackend;
use crate::query_graph::LipDatabase;

pub const CHANNEL_CAPACITY: usize = 64;

// ─── Job ──────────────────────────────────────────────────────────────────────

/// Work item sent from a session to the Tier 2 manager.
#[derive(Debug)]
pub struct VerificationJob {
    /// `file://` URI of the Rust source file.
    pub uri:            String,
    /// Full source text (same as what was sent in the Delta).
    pub source:         String,
    /// Repo root used to initialise rust-analyzer's workspace.
    pub workspace_root: Option<PathBuf>,
    /// Delta sequence number, reused as the LSP document version.
    pub version:        i32,
}

// ─── Manager ─────────────────────────────────────────────────────────────────

pub struct Tier2Manager {
    db:        Arc<Mutex<LipDatabase>>,
    rx:        mpsc::Receiver<VerificationJob>,
    backend:   Option<RustAnalyzerBackend>,
    workspace: Option<PathBuf>,
}

impl Tier2Manager {
    pub fn new(db: Arc<Mutex<LipDatabase>>, rx: mpsc::Receiver<VerificationJob>) -> Self {
        Self {
            db,
            rx,
            backend:   None,
            workspace: None,
        }
    }

    /// Run the manager loop.  Blocks until the sender side of the channel is
    /// dropped (i.e. the daemon shuts down).
    pub async fn run(mut self) {
        info!("tier2 manager started");
        while let Some(job) = self.rx.recv().await {
            self.handle(job).await;
        }
        info!("tier2 manager stopped");
    }

    async fn handle(&mut self, job: VerificationJob) {
        // If the workspace changed, tear down the old backend.
        if let Some(root) = &job.workspace_root {
            if self.workspace.as_deref() != Some(root.as_path()) {
                if self.backend.is_some() {
                    debug!("tier2: workspace changed to {root:?}, reinitialising backend");
                }
                self.workspace = Some(root.clone());
                self.backend   = None;
            }
        }

        // Lazy-init the backend on first use.
        if self.backend.is_none() {
            let workspace = match &self.workspace {
                Some(w) => w.clone(),
                None => {
                    debug!("tier2: no workspace root for {}, skipping", job.uri);
                    return;
                }
            };

            // Only attempt to start rust-analyzer once. If it fails (not
            // installed, bad workspace), disable Tier 2 permanently for this
            // workspace by clearing workspace so we don't retry on every file.
            match RustAnalyzerBackend::new(workspace).await {
                Ok(b) => {
                    info!("tier2: rust-analyzer backend ready");
                    self.backend = Some(b);
                }
                Err(e) => {
                    warn!("tier2: rust-analyzer unavailable, disabling: {e}");
                    self.workspace = None; // prevents infinite retry
                    return;
                }
            }
        }

        let backend = self.backend.as_mut().unwrap();
        match backend.verify_file(&job.uri, &job.source, job.version).await {
            Ok(result) => {
                let upgraded = result.symbols.len();
                let mut db = self.db.lock().await;
                db.upgrade_file_symbols(&result.uri, &result.symbols);
                debug!("tier2: upgraded {upgraded} symbols for {}", job.uri);
            }
            Err(e) => {
                error!("tier2: verification failed for {}: {e}", job.uri);
                // Assume backend is in a bad state; reset it so we reinitialise
                // on the next job (rust-analyzer may have crashed).
                self.backend = None;
            }
        }
    }
}
