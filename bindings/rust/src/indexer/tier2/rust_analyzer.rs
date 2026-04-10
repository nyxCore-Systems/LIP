//! rust-analyzer Tier 2 backend.
//!
//! Spawns `rust-analyzer` as a child process and communicates with it over the
//! Language Server Protocol. For each Rust file, it:
//!
//! 1. Sends `textDocument/didOpen` with the full source.
//! 2. Waits for rust-analyzer to complete its analysis pass.
//! 3. Calls `textDocument/documentSymbol` to obtain the typed symbol list.
//! 4. Calls `textDocument/hover` at each symbol's definition site to extract
//!    the type signature from the hover markdown.
//! 5. Returns a [`VerificationResult`] with confidence scores upgraded to 70.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use serde_json::{json, Value};
use tokio::process::{Child, Command};
use tracing::{debug, info};

use crate::schema::{OwnedSymbolInfo, SymbolKind};

use super::lsp_client::LspClient;

// ─── Public types ─────────────────────────────────────────────────────────────

/// Upgraded symbols and documentation produced by Tier 2 verification.
pub struct VerificationResult {
    pub uri:     String,
    /// Symbols with `confidence_score = 70` and `signature` populated where
    /// rust-analyzer returned hover type information.
    pub symbols: Vec<OwnedSymbolInfo>,
}

// ─── Backend ──────────────────────────────────────────────────────────────────

pub struct RustAnalyzerBackend {
    _child:   Child,
    client:   LspClient,
    _workspace: PathBuf,
    opened:   HashSet<String>,
}

impl RustAnalyzerBackend {
    /// Spawn rust-analyzer, initialize the LSP session for `workspace`, and
    /// wait for the initial analysis pass to complete.
    pub async fn new(workspace: PathBuf) -> anyhow::Result<Self> {
        let mut child = Command::new("rust-analyzer")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null()) // suppress diagnostic noise on the terminal
            .kill_on_drop(true)
            .spawn()
            .context(
                "rust-analyzer not found — install it with `rustup component add rust-analyzer`",
            )?;

        let stdin  = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let client = LspClient::new(stdin, stdout);

        let mut ra = Self {
            _child:   child,
            client,
            _workspace: workspace.clone(),
            opened:   HashSet::new(),
        };

        ra.initialize(&workspace).await?;
        Ok(ra)
    }

    // ── Private: LSP lifecycle ────────────────────────────────────────────────

    async fn initialize(&mut self, workspace: &Path) -> anyhow::Result<()> {
        let root_uri = format!("file://{}", workspace.display());

        let result = self.client.request("initialize", json!({
            "rootUri":  root_uri,
            "rootPath": workspace.display().to_string(),
            "capabilities": {
                "textDocument": {
                    "hover": {
                        "contentFormat": ["markdown", "plaintext"]
                    },
                    "documentSymbol": {
                        // Request flat SymbolInformation[] rather than nested DocumentSymbol[].
                        "hierarchicalDocumentSymbolSupport": false
                    }
                },
                "window": {
                    // We do NOT advertise workDoneProgress support so rust-analyzer
                    // won't send create requests for every analysis step.
                    "workDoneProgress": false
                }
            },
            "initializationOptions": {
                // Speed up startup: disable build scripts and proc-macros.
                "cargo":       { "buildScripts": { "enable": false } },
                "procMacro":   { "enable": false },
                "checkOnSave": { "enable": false }
            }
        }))
        .await?;

        info!(
            "rust-analyzer initialized (server: {:?})",
            result.get("serverInfo").and_then(|v| v.get("name"))
        );

        self.client.notify("initialized", json!({})).await?;

        // Give rust-analyzer time for the initial workspace scan.
        tokio::time::sleep(Duration::from_millis(500)).await;
        Ok(())
    }

    // ── Public: per-file operations ───────────────────────────────────────────

    /// Open `uri` (first time) or update its content (subsequent calls).
    async fn sync_file(&mut self, uri: &str, source: &str, version: i32) -> anyhow::Result<()> {
        if self.opened.contains(uri) {
            self.client.notify("textDocument/didChange", json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": source }]
            })).await?;
        } else {
            self.client.notify("textDocument/didOpen", json!({
                "textDocument": {
                    "uri":        uri,
                    "languageId": "rust",
                    "version":    version,
                    "text":       source
                }
            })).await?;
            self.opened.insert(uri.to_owned());
        }
        // Allow rust-analyzer to process the change before querying.
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok(())
    }

    async fn document_symbols(&mut self, uri: &str) -> anyhow::Result<Vec<RawSymbol>> {
        let result = self.client.request(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": uri } }),
        ).await?;

        let Value::Array(items) = result else { return Ok(vec![]); };

        let mut out = vec![];
        for item in items {
            let name = item.get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            if name.is_empty() { continue; }

            let kind = item.get("kind")
                .and_then(|v| v.as_u64())
                .unwrap_or(0);

            // SymbolInformation has "location.range"; DocumentSymbol has "range" directly.
            let range_ptr = item.pointer("/location/range")
                .or_else(|| item.get("range"));
            let line = range_ptr
                .and_then(|r| r.pointer("/start/line"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            let col = range_ptr
                .and_then(|r| r.pointer("/start/character"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;

            out.push(RawSymbol { name, kind, line, col });
        }
        Ok(out)
    }

    async fn hover_signature(&mut self, uri: &str, line: u32, col: u32) -> anyhow::Result<Option<String>> {
        let result = self.client.request(
            "textDocument/hover",
            json!({
                "textDocument": { "uri": uri },
                "position":     { "line": line, "character": col }
            }),
        ).await?;

        if result.is_null() { return Ok(None); }

        let md = result.pointer("/contents/value")
            .or_else(|| result.pointer("/contents"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        Ok(extract_code_block(md))
    }

    // ── Public: full verification ─────────────────────────────────────────────

    /// Run Tier 2 verification on a single file.
    ///
    /// Syncs the file to rust-analyzer, queries the symbol list, enriches each
    /// symbol with its hover signature, and returns upgraded `OwnedSymbolInfo`s
    /// with `confidence_score = 70`.
    pub async fn verify_file(
        &mut self,
        uri:     &str,
        source:  &str,
        version: i32,
    ) -> anyhow::Result<VerificationResult> {
        self.sync_file(uri, source, version).await?;

        let raw = self.document_symbols(uri).await?;
        debug!("tier2: {} raw symbols for {uri}", raw.len());

        let mut symbols = Vec::with_capacity(raw.len());
        for sym in &raw {
            let sig = self.hover_signature(uri, sym.line, sym.col).await
                .ok()
                .flatten();

            // Build a LIP URI matching the Tier 1 scheme: lip://local/<path>#<name>
            let path = uri.strip_prefix("file://").unwrap_or(uri);
            let sym_uri = format!("lip://local/{path}#{}", sym.name);

            symbols.push(OwnedSymbolInfo {
                uri:              sym_uri,
                display_name:     sym.name.clone(),
                kind:             lsp_kind_to_lip(sym.kind),
                documentation:    None,
                signature:        sig,
                confidence_score: 70,
                relationships:    vec![],
                runtime_p99_ms:   None,
                call_rate_per_s:  None,
                taint_labels:     vec![],
                blast_radius:     0,
            });
        }

        Ok(VerificationResult { uri: uri.to_owned(), symbols })
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

struct RawSymbol {
    name: String,
    kind: u64,
    line: u32,
    col:  u32,
}

/// Extract the first fenced code block from LSP hover markdown.
///
/// rust-analyzer hover looks like:
/// ```text
/// ```rust
/// pub fn foo(x: i32) -> Bar
/// ```
/// Doc comment…
/// ```
fn extract_code_block(md: &str) -> Option<String> {
    let fence_start = md.find("```")?;
    let after_fence = &md[fence_start + 3..];
    // Skip the optional language tag (e.g. "rust\n").
    let body_start = after_fence.find('\n').map(|i| i + 1).unwrap_or(0);
    let body = &after_fence[body_start..];
    let fence_end = body.find("```")?;
    let sig = body[..fence_end].trim().to_owned();
    if sig.is_empty() { None } else { Some(sig) }
}

/// Map LSP `SymbolKind` integer values to LIP `SymbolKind`.
fn lsp_kind_to_lip(kind: u64) -> SymbolKind {
    match kind {
        3  => SymbolKind::Namespace,
        4  => SymbolKind::Namespace,   // Package
        5  => SymbolKind::Class,
        6  => SymbolKind::Method,
        7  => SymbolKind::Field,
        8  => SymbolKind::Constructor,
        9  => SymbolKind::Enum,
        10 => SymbolKind::Interface,
        11 => SymbolKind::Function,
        12 => SymbolKind::Variable,    // Constant
        13 => SymbolKind::Variable,
        14 => SymbolKind::Variable,    // String
        22 => SymbolKind::EnumMember,
        25 => SymbolKind::TypeAlias,
        _  => SymbolKind::Unknown,
    }
}
