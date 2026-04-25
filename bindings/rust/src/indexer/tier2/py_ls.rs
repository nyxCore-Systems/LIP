//! Python language server Tier 2 backend.
//!
//! Tries `pyright-langserver --stdio` first; falls back to `pylsp` if pyright
//! is not installed. Communicates over the Language Server Protocol.
//!
//! For each Python file, it:
//!
//! 1. Sends `textDocument/didOpen` with the full source.
//! 2. Waits briefly for the server to process the change.
//! 3. Calls `textDocument/documentSymbol` to obtain the typed symbol list.
//! 4. Calls `textDocument/hover` at each symbol's definition site to extract
//!    the type signature from the hover markdown.
//! 5. Calls `textDocument/typeDefinition` to record cross-file type relationships.
//! 6. Returns a [`VerificationResult`] with confidence scores upgraded to 90.

use std::collections::HashSet;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use serde_json::{json, Value};
use tokio::process::{Child, Command};
use tracing::{debug, info, warn};

use crate::indexer::language::Language;
use crate::schema::{OwnedRelationship, OwnedSymbolInfo, SymbolKind};

use super::enrich::enrich_v23;
use super::lsp_client::LspClient;
use super::rust_analyzer::{file_uri_to_lip_uri, VerificationResult};

// ─── Backend ──────────────────────────────────────────────────────────────────

pub struct PythonBackend {
    _child: Child,
    client: LspClient,
    opened: HashSet<String>,
}

impl PythonBackend {
    /// Spawn a Python language server (pyright-langserver or pylsp), initialize
    /// the LSP session, and return a ready backend.
    ///
    /// Returns `Err` if neither binary is found in PATH (graceful degradation).
    pub async fn new() -> anyhow::Result<Self> {
        // Try pyright first; fall back to pylsp.
        let (mut child, use_pyright) = try_spawn_python_ls()?;

        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let client = LspClient::new(stdin, stdout);

        let mut backend = Self {
            _child: child,
            client,
            opened: HashSet::new(),
        };

        backend.initialize(use_pyright).await?;
        Ok(backend)
    }

    // ── Private: LSP lifecycle ────────────────────────────────────────────────

    async fn initialize(&mut self, use_pyright: bool) -> anyhow::Result<()> {
        let init_options = if use_pyright {
            json!({ "pythonVersion": "3.11" })
        } else {
            json!({})
        };

        let result = self
            .client
            .request(
                "initialize",
                json!({
                    "rootUri":  null,
                    "rootPath": null,
                    "capabilities": {
                        "textDocument": {
                            "hover": {
                                "contentFormat": ["markdown", "plaintext"]
                            },
                            "documentSymbol": {
                                "hierarchicalDocumentSymbolSupport": false
                            }
                        },
                        "window": {
                            "workDoneProgress": false
                        }
                    },
                    "initializationOptions": init_options
                }),
            )
            .await?;

        let server_name = if use_pyright {
            "pyright-langserver"
        } else {
            "pylsp"
        };
        info!(
            "{server_name} initialized (server: {:?})",
            result.get("serverInfo").and_then(|v| v.get("name"))
        );

        self.client.notify("initialized", json!({})).await?;

        // Give the server a moment to finish startup.
        tokio::time::sleep(Duration::from_millis(300)).await;
        Ok(())
    }

    // ── Private: per-file operations ──────────────────────────────────────────

    async fn sync_file(&mut self, uri: &str, source: &str, version: i32) -> anyhow::Result<()> {
        if self.opened.contains(uri) {
            self.client
                .notify(
                    "textDocument/didChange",
                    json!({
                        "textDocument": { "uri": uri, "version": version },
                        "contentChanges": [{ "text": source }]
                    }),
                )
                .await?;
        } else {
            self.client
                .notify(
                    "textDocument/didOpen",
                    json!({
                        "textDocument": {
                            "uri":        uri,
                            "languageId": "python",
                            "version":    version,
                            "text":       source
                        }
                    }),
                )
                .await?;
            self.opened.insert(uri.to_owned());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok(())
    }

    async fn document_symbols(&mut self, uri: &str) -> anyhow::Result<Vec<RawSymbol>> {
        let result = self
            .client
            .request(
                "textDocument/documentSymbol",
                json!({ "textDocument": { "uri": uri } }),
            )
            .await?;

        let Value::Array(items) = result else {
            return Ok(vec![]);
        };

        let mut out = vec![];
        for item in &items {
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_owned();
            if name.is_empty() {
                continue;
            }

            let kind = item.get("kind").and_then(|v| v.as_u64()).unwrap_or(0);

            // SymbolInformation has "location.range"; DocumentSymbol has "range" directly.
            let range_ptr = item
                .pointer("/location/range")
                .or_else(|| item.get("range"));
            let line = range_ptr
                .and_then(|r| r.pointer("/start/line"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;
            let col = range_ptr
                .and_then(|r| r.pointer("/start/character"))
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32;

            let container = item
                .get("containerName")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_owned);

            out.push(RawSymbol {
                name,
                kind,
                line,
                col,
                container,
            });
        }
        Ok(out)
    }

    async fn hover_signature(
        &mut self,
        uri: &str,
        line: u32,
        col: u32,
    ) -> anyhow::Result<Option<String>> {
        let result = self
            .client
            .request(
                "textDocument/hover",
                json!({
                    "textDocument": { "uri": uri },
                    "position":     { "line": line, "character": col }
                }),
            )
            .await?;

        if result.is_null() {
            return Ok(None);
        }

        let md = result
            .pointer("/contents/value")
            .or_else(|| result.pointer("/contents"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        Ok(extract_py_signature(md))
    }

    /// Resolve the canonical definition URI of a symbol's *type* (cross-file).
    async fn type_definition_uri(
        &mut self,
        uri: &str,
        line: u32,
        col: u32,
    ) -> anyhow::Result<Option<String>> {
        let result = self
            .client
            .request(
                "textDocument/typeDefinition",
                json!({
                    "textDocument": { "uri": uri },
                    "position":     { "line": line, "character": col }
                }),
            )
            .await?;

        let loc = if result.is_array() {
            result.get(0).cloned()
        } else if result.is_object() {
            Some(result.clone())
        } else {
            None
        };

        Ok(loc
            .and_then(|l| {
                l.pointer("/uri")
                    .or_else(|| l.pointer("/targetUri"))
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
            })
            .filter(|def_uri| def_uri != uri)
            .map(|def_uri| file_uri_to_lip_uri(&def_uri)))
    }

    // ── Public: full verification ─────────────────────────────────────────────

    /// Run Tier 2 verification on a single Python file.
    pub async fn verify_file(
        &mut self,
        uri: &str,
        source: &str,
        version: i32,
    ) -> anyhow::Result<VerificationResult> {
        self.sync_file(uri, source, version).await?;

        let raw = self.document_symbols(uri).await?;
        debug!("tier2(py): {} raw symbols for {uri}", raw.len());

        let path = uri.strip_prefix("file://").unwrap_or(uri);

        let mut symbols = Vec::with_capacity(raw.len());
        for sym in &raw {
            let sig = self
                .hover_signature(uri, sym.line, sym.col)
                .await
                .ok()
                .flatten();

            let type_rel = self
                .type_definition_uri(uri, sym.line, sym.col)
                .await
                .ok()
                .flatten()
                .map(|tdef_uri| OwnedRelationship {
                    target_uri: tdef_uri,
                    is_type_definition: true,
                    is_reference: false,
                    is_implementation: false,
                    is_override: false,
                });

            let sym_uri = format!("lip://local/{path}#{}", sym.name);

            // Python convention: names starting with _ are private.
            let is_exported = !sym.name.starts_with('_');
            let mut info = OwnedSymbolInfo {
                uri: sym_uri,
                display_name: sym.name.clone(),
                kind: lsp_kind_to_lip(sym.kind),
                documentation: None,
                signature: sig.clone(),
                confidence_score: 90,
                relationships: type_rel.into_iter().collect(),
                runtime_p99_ms: None,
                call_rate_per_s: None,
                taint_labels: vec![],
                blast_radius: 0,
                is_exported,
                ..Default::default()
            };
            enrich_v23(
                &mut info,
                sig.as_deref(),
                sym.container.clone(),
                Language::Python,
            );
            symbols.push(info);
        }

        Ok(VerificationResult {
            uri: uri.to_owned(),
            symbols,
        })
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

struct RawSymbol {
    name: String,
    kind: u64,
    line: u32,
    col: u32,
    container: Option<String>,
}

/// Try to spawn `pyright-langserver --stdio`; if not found, try `pylsp`.
///
/// Returns `(child, true)` for pyright and `(child, false)` for pylsp.
fn try_spawn_python_ls() -> anyhow::Result<(tokio::process::Child, bool)> {
    match Command::new("pyright-langserver")
        .arg("--stdio")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(child) => Ok((child, true)),
        Err(_) => {
            warn!("pyright-langserver not found, trying pylsp");
            let child = Command::new("pylsp")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .context(
                    "neither pyright-langserver nor pylsp found — install one with \
                     `pip install pyright` or `pip install python-lsp-server`",
                )?;
            Ok((child, false))
        }
    }
}

/// Extract a type signature from Python hover markdown.
///
/// Pyright typically wraps the signature in a fenced code block; pylsp may
/// return plain text. Try the fenced block first, then fall back to the first
/// non-empty line.
fn extract_py_signature(md: &str) -> Option<String> {
    if let Some(fence_start) = md.find("```") {
        let after_fence = &md[fence_start + 3..];
        let body_start = after_fence.find('\n').map(|i| i + 1).unwrap_or(0);
        let body = &after_fence[body_start..];
        if let Some(fence_end) = body.find("```") {
            let sig = body[..fence_end].trim().to_owned();
            if !sig.is_empty() {
                return Some(sig);
            }
        }
    }

    md.lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .map(|l| l.to_owned())
}

/// Map LSP `SymbolKind` integer values to LIP `SymbolKind`.
///
/// Python-relevant kinds: 5 = Class, 11 = Function, 13 = Variable, 2 = Module.
fn lsp_kind_to_lip(kind: u64) -> SymbolKind {
    match kind {
        2 => SymbolKind::Namespace, // Module
        3 => SymbolKind::Namespace, // Namespace
        5 => SymbolKind::Class,
        6 => SymbolKind::Method,
        7 => SymbolKind::Field,
        9 => SymbolKind::Enum,
        10 => SymbolKind::Interface,
        11 => SymbolKind::Function,
        12 => SymbolKind::Variable, // Constant
        13 => SymbolKind::Variable,
        22 => SymbolKind::EnumMember,
        _ => SymbolKind::Unknown,
    }
}
