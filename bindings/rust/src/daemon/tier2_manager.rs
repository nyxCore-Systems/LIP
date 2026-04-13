//! Background Tier 2 verification manager.
//!
//! [`Tier2Manager`] runs as a dedicated tokio task alongside the accept loop.
//! Sessions push [`VerificationJob`]s onto a bounded channel; the manager
//! processes them one at a time, routing each job to the appropriate language
//! server backend based on file extension.
//!
//! The channel is bounded (capacity 64) so that a slow language server does not
//! cause unbounded memory growth. When the channel is full, [`try_send`] in
//! the session drops the job silently — Tier 1 results remain available.

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, Mutex};
use tracing::{debug, error, info, warn};

use crate::indexer::tier2::clangd::ClangdBackend;
use crate::indexer::tier2::dart_ls::DartBackend;
use crate::indexer::tier2::py_ls::PythonBackend;
use crate::indexer::tier2::rust_analyzer::RustAnalyzerBackend;
use crate::indexer::tier2::ts_server::TypeScriptBackend;
use crate::query_graph::{LipDatabase, ServerMessage};

pub const CHANNEL_CAPACITY: usize = 64;

// ─── Job ──────────────────────────────────────────────────────────────────────

/// Work item sent from a session to the Tier 2 manager.
#[derive(Debug)]
pub struct VerificationJob {
    /// `file://` URI of the source file.
    pub uri: String,
    /// Full source text (same as what was sent in the Delta).
    pub source: String,
    /// Repo root used to initialise rust-analyzer's workspace (Rust files only).
    pub workspace_root: Option<PathBuf>,
    /// Delta sequence number, reused as the LSP document version.
    pub version: i32,
}

// ─── Per-language backend state ───────────────────────────────────────────────

/// Holds an optional instance of each language server backend.
///
/// `None` means either "not yet started" OR "permanently disabled" (spawn
/// failed). The `disabled_*` sentinels distinguish the two states so we don't
/// retry a binary that is not installed.
struct Tier2Backends {
    rust: Option<RustAnalyzerBackend>,
    rust_ws: Option<PathBuf>, // workspace last used to init rust backend
    rust_disabled: bool,

    typescript: Option<TypeScriptBackend>,
    typescript_disabled: bool,

    python: Option<PythonBackend>,
    python_disabled: bool,

    dart: Option<DartBackend>,
    dart_disabled: bool,

    clangd: Option<ClangdBackend>,
    clangd_disabled: bool,
}

impl Tier2Backends {
    fn new() -> Self {
        Self {
            rust: None,
            rust_ws: None,
            rust_disabled: false,
            typescript: None,
            typescript_disabled: false,
            python: None,
            python_disabled: false,
            dart: None,
            dart_disabled: false,
            clangd: None,
            clangd_disabled: false,
        }
    }
}

// ─── Manager ─────────────────────────────────────────────────────────────────

pub struct Tier2Manager {
    db: Arc<Mutex<LipDatabase>>,
    rx: mpsc::Receiver<VerificationJob>,
    backends: Tier2Backends,
    /// Broadcast sender for push notifications. `None` when notifications are disabled.
    notify_tx: Option<broadcast::Sender<ServerMessage>>,
}

impl Tier2Manager {
    pub fn new(
        db: Arc<Mutex<LipDatabase>>,
        rx: mpsc::Receiver<VerificationJob>,
        notify_tx: broadcast::Sender<ServerMessage>,
    ) -> Self {
        Self {
            db,
            rx,
            backends: Tier2Backends::new(),
            notify_tx: Some(notify_tx),
        }
    }

    /// Run the manager loop. Blocks until the sender side of the channel is
    /// dropped (i.e. the daemon shuts down).
    pub async fn run(mut self) {
        info!("tier2 manager started");
        while let Some(job) = self.rx.recv().await {
            self.handle(job).await;
        }
        info!("tier2 manager stopped");
    }

    async fn handle(&mut self, job: VerificationJob) {
        if job.uri.ends_with(".rs") {
            self.handle_rust(job).await;
        } else if job.uri.ends_with(".ts") || job.uri.ends_with(".tsx") {
            self.handle_typescript(job).await;
        } else if job.uri.ends_with(".py") {
            self.handle_python(job).await;
        } else if job.uri.ends_with(".dart") {
            self.handle_dart(job).await;
        } else if job.uri.ends_with(".c")
            || job.uri.ends_with(".h")
            || job.uri.ends_with(".cpp")
            || job.uri.ends_with(".cc")
            || job.uri.ends_with(".cxx")
            || job.uri.ends_with(".hpp")
            || job.uri.ends_with(".hxx")
        {
            self.handle_clangd(job).await;
        }
        // Unknown extension — nothing to do; Tier 1 results remain.
    }

    // ── Rust ──────────────────────────────────────────────────────────────────

    async fn handle_rust(&mut self, job: VerificationJob) {
        if self.backends.rust_disabled {
            return;
        }

        // If the workspace changed, tear down the old backend.
        if let Some(root) = &job.workspace_root {
            if self.backends.rust_ws.as_deref() != Some(root.as_path()) {
                if self.backends.rust.is_some() {
                    debug!("tier2: workspace changed to {root:?}, reinitialising rust backend");
                }
                self.backends.rust_ws = Some(root.clone());
                self.backends.rust = None;
            }
        }

        // Lazy-init.
        if self.backends.rust.is_none() {
            let workspace = match &self.backends.rust_ws {
                Some(w) => w.clone(),
                None => {
                    debug!("tier2: no workspace root for {}, skipping", job.uri);
                    return;
                }
            };

            match RustAnalyzerBackend::new(workspace).await {
                Ok(b) => {
                    info!("tier2: rust-analyzer backend ready");
                    self.backends.rust = Some(b);
                }
                Err(e) => {
                    warn!("tier2: rust-analyzer unavailable, disabling: {e}");
                    self.backends.rust_disabled = true;
                    self.backends.rust_ws = None;
                    return;
                }
            }
        }

        let backend = self.backends.rust.as_mut().unwrap();
        match backend
            .verify_file(&job.uri, &job.source, job.version)
            .await
        {
            Ok(result) => {
                let upgraded = result.symbols.len();
                let mut db = self.db.lock().await;
                self.broadcast_upgrades(&result.uri, &result.symbols, &mut db);
                db.upgrade_file_symbols(&result.uri, &result.symbols);
                debug!("tier2: upgraded {upgraded} symbols for {}", job.uri);
            }
            Err(e) => {
                error!("tier2: rust verification failed for {}: {e}", job.uri);
                // Assume backend crashed; reset so we reinitialise on next job.
                self.backends.rust = None;
            }
        }
    }

    // ── TypeScript ────────────────────────────────────────────────────────────

    async fn ensure_ts_backend(&mut self) {
        if self.backends.typescript.is_some() || self.backends.typescript_disabled {
            return;
        }

        match TypeScriptBackend::new().await {
            Ok(b) => {
                info!("tier2: typescript-language-server backend ready");
                self.backends.typescript = Some(b);
            }
            Err(e) => {
                warn!("tier2: typescript-language-server unavailable, disabling: {e}");
                self.backends.typescript_disabled = true;
            }
        }
    }

    async fn handle_typescript(&mut self, job: VerificationJob) {
        if self.backends.typescript_disabled {
            return;
        }

        self.ensure_ts_backend().await;
        if self.backends.typescript_disabled {
            return;
        }

        let backend = self.backends.typescript.as_mut().unwrap();
        match backend
            .verify_file(&job.uri, &job.source, job.version)
            .await
        {
            Ok(result) => {
                let upgraded = result.symbols.len();
                let mut db = self.db.lock().await;
                self.broadcast_upgrades(&result.uri, &result.symbols, &mut db);
                db.upgrade_file_symbols(&result.uri, &result.symbols);
                debug!("tier2: upgraded {upgraded} symbols for {}", job.uri);
            }
            Err(e) => {
                error!("tier2: typescript verification failed for {}: {e}", job.uri);
                self.backends.typescript = None;
            }
        }
    }

    // ── Python ────────────────────────────────────────────────────────────────

    async fn ensure_python_backend(&mut self) {
        if self.backends.python.is_some() || self.backends.python_disabled {
            return;
        }

        match PythonBackend::new().await {
            Ok(b) => {
                info!("tier2: python language server backend ready");
                self.backends.python = Some(b);
            }
            Err(e) => {
                warn!("tier2: python language server unavailable, disabling: {e}");
                self.backends.python_disabled = true;
            }
        }
    }

    async fn handle_python(&mut self, job: VerificationJob) {
        if self.backends.python_disabled {
            return;
        }

        self.ensure_python_backend().await;
        if self.backends.python_disabled {
            return;
        }

        let backend = self.backends.python.as_mut().unwrap();
        match backend
            .verify_file(&job.uri, &job.source, job.version)
            .await
        {
            Ok(result) => {
                let upgraded = result.symbols.len();
                let mut db = self.db.lock().await;
                self.broadcast_upgrades(&result.uri, &result.symbols, &mut db);
                db.upgrade_file_symbols(&result.uri, &result.symbols);
                debug!("tier2: upgraded {upgraded} symbols for {}", job.uri);
            }
            Err(e) => {
                error!("tier2: python verification failed for {}: {e}", job.uri);
                self.backends.python = None;
            }
        }
    }

    // ── Dart ──────────────────────────────────────────────────────────────────

    async fn ensure_dart_backend(&mut self) {
        if self.backends.dart.is_some() || self.backends.dart_disabled {
            return;
        }

        match DartBackend::new().await {
            Ok(b) => {
                info!("tier2: dart language-server backend ready");
                self.backends.dart = Some(b);
            }
            Err(e) => {
                warn!("tier2: dart language-server unavailable, disabling: {e}");
                self.backends.dart_disabled = true;
            }
        }
    }

    async fn handle_dart(&mut self, job: VerificationJob) {
        if self.backends.dart_disabled {
            return;
        }

        self.ensure_dart_backend().await;
        if self.backends.dart_disabled {
            return;
        }

        let backend = self.backends.dart.as_mut().unwrap();
        match backend
            .verify_file(&job.uri, &job.source, job.version)
            .await
        {
            Ok(result) => {
                let upgraded = result.symbols.len();
                let mut db = self.db.lock().await;
                self.broadcast_upgrades(&result.uri, &result.symbols, &mut db);
                db.upgrade_file_symbols(&result.uri, &result.symbols);
                debug!("tier2: upgraded {upgraded} symbols for {}", job.uri);
            }
            Err(e) => {
                error!("tier2: dart verification failed for {}: {e}", job.uri);
                self.backends.dart = None;
            }
        }
    }

    // ── C / C++ ───────────────────────────────────────────────────────────────

    async fn ensure_clangd_backend(&mut self, workspace_root: Option<PathBuf>) {
        if self.backends.clangd.is_some() || self.backends.clangd_disabled {
            return;
        }

        match ClangdBackend::new(workspace_root).await {
            Ok(b) => {
                info!("tier2: clangd backend ready");
                self.backends.clangd = Some(b);
            }
            Err(e) => {
                warn!("tier2: clangd unavailable, disabling: {e}");
                self.backends.clangd_disabled = true;
            }
        }
    }

    async fn handle_clangd(&mut self, job: VerificationJob) {
        if self.backends.clangd_disabled {
            return;
        }

        self.ensure_clangd_backend(job.workspace_root.clone()).await;
        if self.backends.clangd_disabled {
            return;
        }

        let backend = self.backends.clangd.as_mut().unwrap();
        match backend
            .verify_file(&job.uri, &job.source, job.version)
            .await
        {
            Ok(result) => {
                let upgraded = result.symbols.len();
                let mut db = self.db.lock().await;
                self.broadcast_upgrades(&result.uri, &result.symbols, &mut db);
                db.upgrade_file_symbols(&result.uri, &result.symbols);
                debug!("tier2: upgraded {upgraded} symbols for {}", job.uri);
            }
            Err(e) => {
                error!("tier2: clangd verification failed for {}: {e}", job.uri);
                self.backends.clangd = None;
            }
        }
    }

    /// For each symbol in `upgrades` that actually raises confidence, broadcast
    /// a `SymbolUpgraded` notification to all connected sessions.
    ///
    /// Called with the db lock held so we can read the current (pre-upgrade)
    /// confidence values before the merge overwrites them.
    fn broadcast_upgrades(
        &self,
        file_uri: &str,
        upgrades: &[crate::schema::OwnedSymbolInfo],
        db: &mut LipDatabase,
    ) {
        let Some(ref tx) = self.notify_tx else {
            return;
        };
        // If no receivers, skip the db read.
        if tx.receiver_count() == 0 {
            return;
        }

        let current_syms = db.file_symbols(file_uri);
        for up in upgrades {
            let old_confidence = current_syms
                .iter()
                .find(|s| s.uri == up.uri)
                .map(|s| s.confidence_score)
                .unwrap_or(0);
            if up.confidence_score > old_confidence {
                let msg = ServerMessage::SymbolUpgraded {
                    uri: up.uri.clone(),
                    old_confidence,
                    new_confidence: up.confidence_score,
                };
                // `send` fails only when there are no receivers; that's fine.
                let _ = tx.send(msg);
            }
        }
    }
}
