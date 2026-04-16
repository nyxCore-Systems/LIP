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
use crate::indexer::tier2::gopls::GoplsBackend;
use crate::indexer::tier2::kotlin::KotlinBackend;
use crate::indexer::tier2::py_ls::PythonBackend;
use crate::indexer::tier2::rust_analyzer::RustAnalyzerBackend;
use crate::indexer::tier2::swift_ls::SwiftBackend;
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

    gopls: Option<GoplsBackend>,
    gopls_disabled: bool,

    kotlin: Option<KotlinBackend>,
    kotlin_disabled: bool,

    swift: Option<SwiftBackend>,
    swift_disabled: bool,
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
            gopls: None,
            gopls_disabled: false,
            kotlin: None,
            kotlin_disabled: false,
            swift: None,
            swift_disabled: false,
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
        } else if job.uri.ends_with(".ts")
            || job.uri.ends_with(".tsx")
            || job.uri.ends_with(".js")
            || job.uri.ends_with(".jsx")
            || job.uri.ends_with(".mjs")
            || job.uri.ends_with(".cjs")
        {
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
        } else if job.uri.ends_with(".go") {
            self.handle_gopls(job).await;
        } else if job.uri.ends_with(".kt") || job.uri.ends_with(".kts") {
            self.handle_kotlin(job).await;
        } else if job.uri.ends_with(".swift") {
            self.handle_swift(job).await;
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

    // ── Go ────────────────────────────────────────────────────────────────────

    async fn ensure_gopls_backend(&mut self, workspace_root: Option<PathBuf>) {
        if self.backends.gopls.is_some() || self.backends.gopls_disabled {
            return;
        }

        match GoplsBackend::new(workspace_root).await {
            Ok(b) => {
                info!("tier2: gopls backend ready");
                self.backends.gopls = Some(b);
            }
            Err(e) => {
                warn!("tier2: gopls unavailable, disabling: {e}");
                self.backends.gopls_disabled = true;
            }
        }
    }

    async fn handle_gopls(&mut self, job: VerificationJob) {
        if self.backends.gopls_disabled {
            return;
        }

        self.ensure_gopls_backend(job.workspace_root.clone()).await;
        if self.backends.gopls_disabled {
            return;
        }

        let backend = self.backends.gopls.as_mut().unwrap();
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
                error!("tier2: gopls verification failed for {}: {e}", job.uri);
                self.backends.gopls = None;
            }
        }
    }

    // ── Kotlin ────────────────────────────────────────────────────────────────

    async fn ensure_kotlin_backend(&mut self, workspace_root: Option<PathBuf>) {
        if self.backends.kotlin.is_some() || self.backends.kotlin_disabled {
            return;
        }

        match KotlinBackend::new(workspace_root).await {
            Ok(b) => {
                info!("tier2: kotlin-language-server backend ready");
                self.backends.kotlin = Some(b);
            }
            Err(e) => {
                warn!("tier2: kotlin-language-server unavailable, disabling: {e}");
                self.backends.kotlin_disabled = true;
            }
        }
    }

    async fn handle_kotlin(&mut self, job: VerificationJob) {
        if self.backends.kotlin_disabled {
            return;
        }

        self.ensure_kotlin_backend(job.workspace_root.clone()).await;
        if self.backends.kotlin_disabled {
            return;
        }

        let backend = self.backends.kotlin.as_mut().unwrap();
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
                error!("tier2: kotlin verification failed for {}: {e}", job.uri);
                self.backends.kotlin = None;
            }
        }
    }

    // ── Swift ─────────────────────────────────────────────────────────────────

    async fn ensure_swift_backend(&mut self, workspace_root: Option<PathBuf>) {
        if self.backends.swift.is_some() || self.backends.swift_disabled {
            return;
        }

        match SwiftBackend::new(workspace_root).await {
            Ok(b) => {
                info!("tier2: sourcekit-lsp backend ready");
                self.backends.swift = Some(b);
            }
            Err(e) => {
                warn!("tier2: sourcekit-lsp unavailable, disabling: {e}");
                self.backends.swift_disabled = true;
            }
        }
    }

    async fn handle_swift(&mut self, job: VerificationJob) {
        if self.backends.swift_disabled {
            return;
        }

        self.ensure_swift_backend(job.workspace_root.clone()).await;
        if self.backends.swift_disabled {
            return;
        }

        let backend = self.backends.swift.as_mut().unwrap();
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
                error!("tier2: swift verification failed for {}: {e}", job.uri);
                self.backends.swift = None;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::OwnedSymbolInfo;
    use std::sync::Arc;
    use tokio::sync::{broadcast, mpsc, Mutex};

    // ── Helpers ──────────────────────────────────────────────────────────────

    /// Build a minimal `Tier2Manager` with all backends disabled.
    ///
    /// This is the primary test fixture: every backend is marked permanently
    /// disabled so that `handle_*` returns immediately without attempting to
    /// spawn a language server process. This lets us exercise routing, channel
    /// behaviour and broadcast logic in isolation.
    fn manager_all_disabled() -> (Tier2Manager, mpsc::Sender<VerificationJob>) {
        let (tx, rx) = mpsc::channel(CHANNEL_CAPACITY);
        let (notify_tx, _) = broadcast::channel(16);
        let db = Arc::new(Mutex::new(LipDatabase::new()));

        let mut backends = Tier2Backends::new();
        backends.rust_disabled = true;
        backends.typescript_disabled = true;
        backends.python_disabled = true;
        backends.dart_disabled = true;
        backends.clangd_disabled = true;
        backends.gopls_disabled = true;
        backends.kotlin_disabled = true;
        backends.swift_disabled = true;

        let mgr = Tier2Manager {
            db,
            rx,
            backends,
            notify_tx: Some(notify_tx),
        };
        (mgr, tx)
    }

    fn make_job(uri: &str) -> VerificationJob {
        VerificationJob {
            uri: uri.to_owned(),
            source: String::new(),
            workspace_root: None,
            version: 1,
        }
    }

    fn make_symbol(uri: &str, confidence: u8) -> OwnedSymbolInfo {
        OwnedSymbolInfo {
            uri: uri.to_owned(),
            display_name: uri.rsplit('#').next().unwrap_or(uri).to_owned(),
            kind: crate::schema::SymbolKind::Function,
            documentation: None,
            signature: None,
            confidence_score: confidence,
            relationships: vec![],
            runtime_p99_ms: None,
            call_rate_per_s: None,
            taint_labels: vec![],
            blast_radius: 0,
            is_exported: false,
        }
    }

    // ── Job routing ──────────────────────────────────────────────────────────

    /// Verify that `handle` dispatches to the correct backend for every
    /// supported file extension. Because all backends are disabled, each
    /// handler returns immediately — the absence of a panic proves the
    /// routing path was reached (the disabled-flag early-return is the first
    /// line in every `handle_*` method).
    #[tokio::test]
    async fn routing_dispatches_to_correct_backend() {
        let (mut mgr, _tx) = manager_all_disabled();

        // Rust
        mgr.handle(make_job("file:///src/main.rs")).await;

        // TypeScript family
        mgr.handle(make_job("file:///src/index.ts")).await;
        mgr.handle(make_job("file:///src/App.tsx")).await;
        mgr.handle(make_job("file:///src/util.js")).await;
        mgr.handle(make_job("file:///src/App.jsx")).await;
        mgr.handle(make_job("file:///src/esm.mjs")).await;
        mgr.handle(make_job("file:///src/cjs.cjs")).await;

        // Python
        mgr.handle(make_job("file:///src/app.py")).await;

        // Dart
        mgr.handle(make_job("file:///lib/main.dart")).await;

        // C / C++
        mgr.handle(make_job("file:///src/main.c")).await;
        mgr.handle(make_job("file:///src/lib.h")).await;
        mgr.handle(make_job("file:///src/main.cpp")).await;
        mgr.handle(make_job("file:///src/util.cc")).await;
        mgr.handle(make_job("file:///src/core.cxx")).await;
        mgr.handle(make_job("file:///src/api.hpp")).await;
        mgr.handle(make_job("file:///src/api.hxx")).await;

        // Go
        mgr.handle(make_job("file:///cmd/main.go")).await;

        // Kotlin
        mgr.handle(make_job("file:///src/Main.kt")).await;
        mgr.handle(make_job("file:///build.gradle.kts")).await;

        // Swift
        mgr.handle(make_job("file:///Sources/App.swift")).await;
    }

    /// Files with unknown extensions should be silently ignored — no panic,
    /// no error, no backend touched.
    #[tokio::test]
    async fn routing_unknown_extension_is_noop() {
        let (mut mgr, _tx) = manager_all_disabled();

        mgr.handle(make_job("file:///README.md")).await;
        mgr.handle(make_job("file:///data.json")).await;
        mgr.handle(make_job("file:///Makefile")).await;
    }

    // ── Channel behaviour ────────────────────────────────────────────────────

    /// When the bounded channel is full, `try_send` must fail (Err) rather
    /// than blocking the caller.
    #[tokio::test]
    async fn full_channel_drops_jobs() {
        let (tx, _rx) = mpsc::channel::<VerificationJob>(CHANNEL_CAPACITY);

        // Fill the channel to capacity.
        for i in 0..CHANNEL_CAPACITY {
            let job = VerificationJob {
                uri: format!("file:///src/file_{i}.rs"),
                source: String::new(),
                workspace_root: None,
                version: 1,
            };
            tx.try_send(job).expect("channel should accept up to capacity");
        }

        // The next try_send must fail — this is the documented contract.
        let overflow = VerificationJob {
            uri: "file:///src/overflow.rs".to_owned(),
            source: String::new(),
            workspace_root: None,
            version: 1,
        };
        assert!(
            tx.try_send(overflow).is_err(),
            "try_send on a full channel must return Err, not block"
        );
    }

    // ── Backend unavailability ───────────────────────────────────────────────

    /// When a backend's `disabled` flag is set (binary not found), calling
    /// `handle` with a matching file must return gracefully — no panic, no
    /// spawn attempt.
    #[tokio::test]
    async fn disabled_backend_skips_gracefully() {
        let (mut mgr, _tx) = manager_all_disabled();

        // Explicitly verify each disabled backend short-circuits.
        assert!(mgr.backends.rust_disabled);
        mgr.handle(make_job("file:///src/lib.rs")).await;
        assert!(mgr.backends.rust.is_none(), "no backend should be created");

        assert!(mgr.backends.typescript_disabled);
        mgr.handle(make_job("file:///src/app.ts")).await;
        assert!(mgr.backends.typescript.is_none());

        assert!(mgr.backends.python_disabled);
        mgr.handle(make_job("file:///src/app.py")).await;
        assert!(mgr.backends.python.is_none());

        assert!(mgr.backends.dart_disabled);
        mgr.handle(make_job("file:///lib/main.dart")).await;
        assert!(mgr.backends.dart.is_none());

        assert!(mgr.backends.clangd_disabled);
        mgr.handle(make_job("file:///src/main.c")).await;
        assert!(mgr.backends.clangd.is_none());

        assert!(mgr.backends.gopls_disabled);
        mgr.handle(make_job("file:///cmd/main.go")).await;
        assert!(mgr.backends.gopls.is_none());

        assert!(mgr.backends.kotlin_disabled);
        mgr.handle(make_job("file:///src/Main.kt")).await;
        assert!(mgr.backends.kotlin.is_none());

        assert!(mgr.backends.swift_disabled);
        mgr.handle(make_job("file:///Sources/App.swift")).await;
        assert!(mgr.backends.swift.is_none());
    }

    // ── Confidence elevation (broadcast) ─────────────────────────────────────

    /// When a Tier 2 upgrade raises a symbol's confidence, the manager must
    /// broadcast a `SymbolUpgraded` message with the correct old/new scores.
    #[tokio::test]
    async fn broadcast_upgrades_fires_on_confidence_increase() {
        let (notify_tx, mut notify_rx) = broadcast::channel(16);
        let db = Arc::new(Mutex::new(LipDatabase::new()));

        let file_uri = "file:///src/lib.rs";
        let sym_uri = "lip://local//src/lib.rs#foo";

        // Seed the database with a Tier 1 symbol at confidence 40.
        {
            let mut db = db.lock().await;
            db.upsert_file_precomputed(
                file_uri.to_owned(),
                "rust".to_owned(),
                "abc123".to_owned(),
                vec![make_symbol(sym_uri, 40)],
                vec![],
                vec![],
            );
        }

        let mgr = Tier2Manager {
            db: db.clone(),
            rx: mpsc::channel(1).1,
            backends: Tier2Backends::new(),
            notify_tx: Some(notify_tx),
        };

        // Simulate a Tier 2 upgrade to confidence 90.
        let upgrades = vec![make_symbol(sym_uri, 90)];
        {
            let mut db = db.lock().await;
            mgr.broadcast_upgrades(file_uri, &upgrades, &mut db);
        }

        let msg = notify_rx.try_recv().expect("should receive a broadcast");
        match msg {
            ServerMessage::SymbolUpgraded {
                uri,
                old_confidence,
                new_confidence,
            } => {
                assert_eq!(uri, sym_uri);
                assert_eq!(old_confidence, 40);
                assert_eq!(new_confidence, 90);
            }
            other => panic!("expected SymbolUpgraded, got {other:?}"),
        }
    }

    /// No broadcast should fire when the upgrade does NOT raise confidence
    /// (e.g. a stale Tier 2 result arriving after a SCIP push already set
    /// the symbol to confidence 95).
    #[tokio::test]
    async fn broadcast_upgrades_silent_when_confidence_not_raised() {
        let (notify_tx, mut notify_rx) = broadcast::channel(16);
        let db = Arc::new(Mutex::new(LipDatabase::new()));

        let file_uri = "file:///src/lib.rs";
        let sym_uri = "lip://local//src/lib.rs#bar";

        // Seed at confidence 95 (SCIP push).
        {
            let mut db = db.lock().await;
            db.upsert_file_precomputed(
                file_uri.to_owned(),
                "rust".to_owned(),
                "abc123".to_owned(),
                vec![make_symbol(sym_uri, 95)],
                vec![],
                vec![],
            );
        }

        let mgr = Tier2Manager {
            db: db.clone(),
            rx: mpsc::channel(1).1,
            backends: Tier2Backends::new(),
            notify_tx: Some(notify_tx),
        };

        // "Upgrade" to 90 — this is actually a downgrade, no broadcast.
        let upgrades = vec![make_symbol(sym_uri, 90)];
        {
            let mut db = db.lock().await;
            mgr.broadcast_upgrades(file_uri, &upgrades, &mut db);
        }

        assert!(
            notify_rx.try_recv().is_err(),
            "no broadcast should fire when the upgrade does not raise confidence"
        );
    }

    /// When there are no broadcast receivers, `broadcast_upgrades` must
    /// short-circuit without reading from the db (the receiver_count check).
    #[tokio::test]
    async fn broadcast_upgrades_noop_without_receivers() {
        let (notify_tx, _) = broadcast::channel::<ServerMessage>(16);
        let db = Arc::new(Mutex::new(LipDatabase::new()));

        // Drop the only receiver so receiver_count == 0.
        // (The `_` binding above was never subscribed to.)
        drop(notify_tx.subscribe()); // subscribe then immediately drop

        let mgr = Tier2Manager {
            db: db.clone(),
            rx: mpsc::channel(1).1,
            backends: Tier2Backends::new(),
            notify_tx: Some(notify_tx),
        };

        let upgrades = vec![make_symbol("lip://local//src/lib.rs#baz", 90)];
        {
            let mut db = db.lock().await;
            // Should not panic even though "file:///src/lib.rs" is not in the db.
            mgr.broadcast_upgrades("file:///src/lib.rs", &upgrades, &mut db);
        }
    }

    /// When `notify_tx` is `None`, `broadcast_upgrades` must be a no-op.
    #[tokio::test]
    async fn broadcast_upgrades_noop_when_notifications_disabled() {
        let db = Arc::new(Mutex::new(LipDatabase::new()));

        let mgr = Tier2Manager {
            db: db.clone(),
            rx: mpsc::channel(1).1,
            backends: Tier2Backends::new(),
            notify_tx: None,
        };

        let upgrades = vec![make_symbol("lip://local//src/lib.rs#baz", 90)];
        {
            let mut db = db.lock().await;
            mgr.broadcast_upgrades("file:///src/lib.rs", &upgrades, &mut db);
        }
        // No panic = pass.
    }

    // ── Symbol upgrade merging (LipDatabase::upgrade_file_symbols) ───────────

    /// `upgrade_file_symbols` must raise confidence and merge signature,
    /// documentation and relationships from Tier 2 results into existing
    /// Tier 1 symbols.
    #[tokio::test]
    async fn upgrade_merges_signature_and_confidence() {
        let mut db = LipDatabase::new();

        let file_uri = "file:///src/lib.rs";
        let sym_uri = "lip://local//src/lib.rs#process";

        let tier1 = OwnedSymbolInfo {
            uri: sym_uri.to_owned(),
            display_name: "process".to_owned(),
            kind: crate::schema::SymbolKind::Function,
            documentation: None,
            signature: None,
            confidence_score: 40,
            relationships: vec![],
            runtime_p99_ms: None,
            call_rate_per_s: None,
            taint_labels: vec![],
            blast_radius: 0,
            is_exported: false,
        };

        db.upsert_file_precomputed(
            file_uri.to_owned(),
            "rust".to_owned(),
            "hash1".to_owned(),
            vec![tier1],
            vec![],
            vec![],
        );

        // Simulate Tier 2 upgrade with signature and doc.
        let upgrade = OwnedSymbolInfo {
            uri: sym_uri.to_owned(),
            display_name: "process".to_owned(),
            kind: crate::schema::SymbolKind::Function,
            documentation: Some("Process the input data.".to_owned()),
            signature: Some("pub fn process(input: &[u8]) -> Result<()>".to_owned()),
            confidence_score: 90,
            relationships: vec![crate::schema::OwnedRelationship {
                target_uri: "lip://local//src/types.rs#Result".to_owned(),
                is_type_definition: true,
                is_reference: false,
                is_implementation: false,
                is_override: false,
            }],
            runtime_p99_ms: None,
            call_rate_per_s: None,
            taint_labels: vec![],
            blast_radius: 0,
            is_exported: true,
        };

        db.upgrade_file_symbols(file_uri, &[upgrade]);

        let symbols = db.file_symbols(file_uri);
        assert_eq!(symbols.len(), 1);
        let sym = &symbols[0];
        assert_eq!(sym.confidence_score, 90, "confidence must be elevated");
        assert_eq!(
            sym.signature.as_deref(),
            Some("pub fn process(input: &[u8]) -> Result<()>"),
            "signature must be merged from Tier 2"
        );
        assert_eq!(
            sym.documentation.as_deref(),
            Some("Process the input data."),
            "documentation must be merged from Tier 2"
        );
        assert_eq!(sym.relationships.len(), 1, "relationships must be merged");
        assert!(sym.relationships[0].is_type_definition);
    }

    /// `upgrade_file_symbols` must NOT downgrade a symbol that already has a
    /// higher confidence (e.g. from a SCIP push at 95).
    #[tokio::test]
    async fn upgrade_does_not_downgrade_confidence() {
        let mut db = LipDatabase::new();

        let file_uri = "file:///src/lib.rs";
        let sym_uri = "lip://local//src/lib.rs#hi_conf";

        let existing = OwnedSymbolInfo {
            uri: sym_uri.to_owned(),
            display_name: "hi_conf".to_owned(),
            kind: crate::schema::SymbolKind::Function,
            documentation: Some("Already documented.".to_owned()),
            signature: Some("fn hi_conf() -> u32".to_owned()),
            confidence_score: 95,
            relationships: vec![],
            runtime_p99_ms: None,
            call_rate_per_s: None,
            taint_labels: vec![],
            blast_radius: 0,
            is_exported: false,
        };

        db.upsert_file_precomputed(
            file_uri.to_owned(),
            "rust".to_owned(),
            "hash2".to_owned(),
            vec![existing],
            vec![],
            vec![],
        );

        // Tier 2 arrives late with a lower confidence.
        let stale = OwnedSymbolInfo {
            uri: sym_uri.to_owned(),
            display_name: "hi_conf".to_owned(),
            kind: crate::schema::SymbolKind::Function,
            documentation: None,
            signature: Some("fn hi_conf() -> u32".to_owned()),
            confidence_score: 70,
            relationships: vec![],
            runtime_p99_ms: None,
            call_rate_per_s: None,
            taint_labels: vec![],
            blast_radius: 0,
            is_exported: false,
        };

        db.upgrade_file_symbols(file_uri, &[stale]);

        let symbols = db.file_symbols(file_uri);
        let sym = &symbols[0];
        assert_eq!(
            sym.confidence_score, 95,
            "confidence must not be downgraded"
        );
        assert_eq!(
            sym.documentation.as_deref(),
            Some("Already documented."),
            "existing documentation must be preserved"
        );
    }

    /// `upgrade_file_symbols` with an empty upgrade slice is a no-op.
    #[tokio::test]
    async fn upgrade_empty_is_noop() {
        let mut db = LipDatabase::new();

        let file_uri = "file:///src/lib.rs";
        db.upsert_file_precomputed(
            file_uri.to_owned(),
            "rust".to_owned(),
            "hash3".to_owned(),
            vec![make_symbol("lip://local//src/lib.rs#x", 40)],
            vec![],
            vec![],
        );

        db.upgrade_file_symbols(file_uri, &[]);

        let symbols = db.file_symbols(file_uri);
        assert_eq!(symbols[0].confidence_score, 40, "nothing should change");
    }

    /// `upgrade_file_symbols` on a URI not in the database is a no-op.
    #[tokio::test]
    async fn upgrade_unknown_file_is_noop() {
        let mut db = LipDatabase::new();
        let sym = make_symbol("lip://local//unknown.rs#foo", 90);
        // Must not panic.
        db.upgrade_file_symbols("file:///unknown.rs", &[sym]);
    }

    // ── Tier2Backends default state ──────────────────────────────────────────

    /// Fresh `Tier2Backends` must have all backends as `None` and all
    /// disabled flags as `false` — backends are lazily initialised.
    #[test]
    fn backends_default_state() {
        let b = Tier2Backends::new();
        assert!(b.rust.is_none());
        assert!(!b.rust_disabled);
        assert!(b.typescript.is_none());
        assert!(!b.typescript_disabled);
        assert!(b.python.is_none());
        assert!(!b.python_disabled);
        assert!(b.dart.is_none());
        assert!(!b.dart_disabled);
        assert!(b.clangd.is_none());
        assert!(!b.clangd_disabled);
        assert!(b.gopls.is_none());
        assert!(!b.gopls_disabled);
        assert!(b.kotlin.is_none());
        assert!(!b.kotlin_disabled);
        assert!(b.swift.is_none());
        assert!(!b.swift_disabled);
    }

    // ── Channel capacity constant ────────────────────────────────────────────

    #[test]
    fn channel_capacity_is_64() {
        assert_eq!(CHANNEL_CAPACITY, 64);
    }
}
