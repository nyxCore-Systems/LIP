//! TypeScript language server Tier 2 backend.
//!
//! Spawns `typescript-language-server --stdio` as a child process and
//! communicates with it over the Language Server Protocol. For each TypeScript
//! file, it:
//!
//! 1. Sends `textDocument/didOpen` with the full source.
//! 2. Waits briefly for the server to process the change.
//! 3. Calls `textDocument/documentSymbol` to obtain the typed symbol list.
//! 4. Calls `textDocument/hover` at each symbol's definition site to extract
//!    the type signature from the hover markdown.
//! 5. Returns a [`VerificationResult`] with confidence scores upgraded to 70.

use std::collections::HashSet;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use serde_json::{json, Value};
use tokio::process::{Child, Command};
use tracing::{debug, info};

use crate::schema::{OwnedSymbolInfo, SymbolKind};

use super::lsp_client::LspClient;
use super::rust_analyzer::VerificationResult;

// ─── Backend ──────────────────────────────────────────────────────────────────

pub struct TypeScriptBackend {
    _child:  Child,
    client:  LspClient,
    opened:  HashSet<String>,
}

impl TypeScriptBackend {
    /// Spawn `typescript-language-server`, initialize the LSP session, and
    /// return a ready backend.
    ///
    /// Returns `Err` if the binary is not found in PATH (graceful degradation).
    pub async fn new() -> anyhow::Result<Self> {
        let mut child = Command::new("typescript-language-server")
            .arg("--stdio")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context(
                "typescript-language-server not found — install it with \
                 `npm install -g typescript-language-server typescript`",
            )?;

        let stdin  = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let client = LspClient::new(stdin, stdout);

        let mut backend = Self {
            _child: child,
            client,
            opened: HashSet::new(),
        };

        backend.initialize().await?;
        Ok(backend)
    }

    // ── Private: LSP lifecycle ────────────────────────────────────────────────

    async fn initialize(&mut self) -> anyhow::Result<()> {
        let result = self.client.request("initialize", json!({
            "rootUri":  null,
            "rootPath": null,
            "capabilities": {
                "textDocument": {
                    "hover": {
                        "contentFormat": ["markdown", "plaintext"]
                    },
                    "documentSymbol": {
                        // Accept both nested DocumentSymbol[] and flat SymbolInformation[].
                        "hierarchicalDocumentSymbolSupport": true
                    }
                },
                "window": {
                    "workDoneProgress": false
                }
            },
            "initializationOptions": {}
        }))
        .await?;

        info!(
            "typescript-language-server initialized (server: {:?})",
            result.get("serverInfo").and_then(|v| v.get("name"))
        );

        self.client.notify("initialized", json!({})).await?;

        // Give the server a moment to finish startup.
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(())
    }

    // ── Private: per-file operations ──────────────────────────────────────────

    async fn sync_file(&mut self, uri: &str, source: &str, version: i32, lang: &str) -> anyhow::Result<()> {
        if self.opened.contains(uri) {
            self.client.notify("textDocument/didChange", json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": source }]
            })).await?;
        } else {
            self.client.notify("textDocument/didOpen", json!({
                "textDocument": {
                    "uri":        uri,
                    "languageId": lang,
                    "version":    version,
                    "text":       source
                }
            })).await?;
            self.opened.insert(uri.to_owned());
        }
        // Allow the server to process the change before querying.
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
        collect_symbols(&items, &mut out);
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

        // typescript-language-server returns markdown; try to extract a code block,
        // otherwise fall back to the first non-empty line.
        let md = result.pointer("/contents/value")
            .or_else(|| result.pointer("/contents"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        Ok(extract_ts_signature(md))
    }

    // ── Public: full verification ─────────────────────────────────────────────

    /// Run Tier 2 verification on a single TypeScript/TSX file.
    pub async fn verify_file(
        &mut self,
        uri:     &str,
        source:  &str,
        version: i32,
    ) -> anyhow::Result<VerificationResult> {
        let lang = if uri.ends_with(".tsx") { "typescriptreact" } else { "typescript" };
        self.sync_file(uri, source, version, lang).await?;

        let raw = self.document_symbols(uri).await?;
        debug!("tier2(ts): {} raw symbols for {uri}", raw.len());

        let mut symbols = Vec::with_capacity(raw.len());
        for sym in &raw {
            let sig = self.hover_signature(uri, sym.line, sym.col).await
                .ok()
                .flatten();

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

/// Recursively collect symbols from either `DocumentSymbol[]` (nested, with
/// `selectionRange`) or `SymbolInformation[]` (flat, with `location.range`).
fn collect_symbols(items: &[Value], out: &mut Vec<RawSymbol>) {
    for item in items {
        let name = item.get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        if name.is_empty() { continue; }

        let kind = item.get("kind")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);

        // DocumentSymbol uses `selectionRange` for the symbol's name position;
        // SymbolInformation uses `location.range`.
        let range_ptr = item.pointer("/selectionRange")
            .or_else(|| item.pointer("/location/range"))
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

        // Recurse into nested DocumentSymbol children.
        if let Some(Value::Array(children)) = item.get("children") {
            collect_symbols(children, out);
        }
    }
}

/// Extract a type signature from TypeScript hover markdown.
///
/// `typescript-language-server` typically returns a fenced typescript code
/// block containing the signature (e.g. `function foo(x: number): string`).
/// Falls back to the first non-empty line if no fenced block is found.
fn extract_ts_signature(md: &str) -> Option<String> {
    // Try fenced code block first (same logic as rust-analyzer helper).
    if let Some(fence_start) = md.find("```") {
        let after_fence = &md[fence_start + 3..];
        let body_start = after_fence.find('\n').map(|i| i + 1).unwrap_or(0);
        let body = &after_fence[body_start..];
        if let Some(fence_end) = body.find("```") {
            let sig = body[..fence_end].trim().to_owned();
            if !sig.is_empty() { return Some(sig); }
        }
    }

    // Fall back to first non-empty line.
    md.lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .map(|l| l.to_owned())
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
