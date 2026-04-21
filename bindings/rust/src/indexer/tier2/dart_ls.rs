//! Dart language server Tier 2 backend.
//!
//! Spawns `dart language-server --protocol=lsp` as a child process (requires
//! Dart SDK ≥ 2.15 in PATH) and communicates over the Language Server Protocol.
//!
//! For each Dart file, it:
//!
//! 1. Sends `textDocument/didOpen` with the full source.
//! 2. Waits briefly for the server to process the change.
//! 3. Calls `textDocument/documentSymbol` to obtain the symbol list.
//! 4. Calls `textDocument/hover` at each symbol's definition site to extract
//!    the type signature from the hover markdown.
//! 5. Calls `textDocument/typeDefinition` to record cross-file type relationships.
//! 6. Returns a [`VerificationResult`] with confidence scores upgraded to 90.
//!
//! If `dart` is not in PATH the backend fails to spawn and is permanently
//! disabled for the session — Tier 1 results remain fully functional.

use std::collections::HashSet;
use std::process::Stdio;
use std::time::Duration;

use anyhow::Context;
use serde_json::{json, Value};
use tokio::process::{Child, Command};
use tracing::{debug, info};

use crate::indexer::language::Language;
use crate::schema::{OwnedRelationship, OwnedSymbolInfo, SymbolKind};

use super::enrich::enrich_v23;
use super::lsp_client::LspClient;
use super::rust_analyzer::{file_uri_to_lip_uri, VerificationResult};

// ─── Backend ──────────────────────────────────────────────────────────────────

pub struct DartBackend {
    _child: Child,
    client: LspClient,
    opened: HashSet<String>,
}

impl DartBackend {
    /// Spawn `dart language-server --protocol=lsp`, initialize the LSP session,
    /// and return a ready backend.
    ///
    /// Returns `Err` if the `dart` SDK binary is not found in PATH.
    pub async fn new() -> anyhow::Result<Self> {
        let mut child = Command::new("dart")
            .args(["language-server", "--protocol=lsp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context(
                "dart language-server not found — install the Dart SDK and ensure \
                 `dart` is in PATH (https://dart.dev/get-dart)",
            )?;

        let stdin = child.stdin.take().context("no stdin")?;
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
                                "hierarchicalDocumentSymbolSupport": true
                            }
                        },
                        "window": {
                            "workDoneProgress": false
                        }
                    },
                    "initializationOptions": {
                        // Suppress analysis-server diagnostics we don't need.
                        "onlyAnalyzeProjectsWithOpenFiles": true,
                        "suggestFromUnimportedLibraries": false
                    }
                }),
            )
            .await?;

        info!(
            "dart language-server initialized (server: {:?})",
            result.get("serverInfo").and_then(|v| v.get("name"))
        );

        self.client.notify("initialized", json!({})).await?;

        // The Dart analysis server takes a moment to initialize its analysis context.
        tokio::time::sleep(Duration::from_millis(500)).await;
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
                            "languageId": "dart",
                            "version":    version,
                            "text":       source
                        }
                    }),
                )
                .await?;
            self.opened.insert(uri.to_owned());
        }
        // Dart's analysis server needs a moment to process before responding
        // to documentSymbol queries accurately.
        tokio::time::sleep(Duration::from_millis(300)).await;
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
        collect_symbols(&items, None, &mut out);
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

        // The Dart analysis server returns markdown; extract the signature from
        // a fenced code block when present, otherwise take the first line.
        let md = result
            .pointer("/contents/value")
            .or_else(|| result.pointer("/contents"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        Ok(extract_dart_signature(md))
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

    /// Run Tier 2 verification on a single Dart file.
    pub async fn verify_file(
        &mut self,
        uri: &str,
        source: &str,
        version: i32,
    ) -> anyhow::Result<VerificationResult> {
        self.sync_file(uri, source, version).await?;

        let raw = self.document_symbols(uri).await?;
        debug!("tier2(dart): {} raw symbols for {uri}", raw.len());

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

            // Dart convention: names starting with _ are library-private.
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
            enrich_v23(&mut info, sig.as_deref(), sym.container.clone(), Language::Dart);
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

/// Recursively collect symbols from `DocumentSymbol[]` (nested) or
/// `SymbolInformation[]` (flat). The Dart analysis server returns the
/// hierarchical form when `hierarchicalDocumentSymbolSupport` is true.
fn collect_symbols(items: &[Value], parent: Option<&str>, out: &mut Vec<RawSymbol>) {
    for item in items {
        let name = item
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        if name.is_empty() {
            continue;
        }

        let kind = item.get("kind").and_then(|v| v.as_u64()).unwrap_or(0);

        // Prefer selectionRange (DocumentSymbol) → location.range (SymbolInformation) → range.
        let range_ptr = item
            .pointer("/selectionRange")
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

        let container = item
            .get("containerName")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| parent.map(str::to_owned));

        out.push(RawSymbol {
            name: name.clone(),
            kind,
            line,
            col,
            container,
        });

        // Recurse into nested children (classes contain methods, etc.).
        if let Some(Value::Array(children)) = item.get("children") {
            collect_symbols(children, Some(&name), out);
        }
    }
}

/// Extract a Dart type signature from hover markdown.
///
/// The Dart analysis server wraps signatures in a fenced `dart` code block.
/// Falls back to the first non-empty line if no fenced block is present.
fn extract_dart_signature(md: &str) -> Option<String> {
    if let Some(fence_start) = md.find("```") {
        let after_fence = &md[fence_start + 3..];
        // Skip the language tag line (e.g. "dart\n").
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
/// Dart-relevant LSP kinds: 5=Class, 6=Method, 8=Constructor, 9=Enum,
/// 11=Function, 13=Variable, 22=EnumMember, 25=TypeParameter.
fn lsp_kind_to_lip(kind: u64) -> SymbolKind {
    match kind {
        2 => SymbolKind::Namespace, // Module
        3 => SymbolKind::Namespace, // Namespace
        5 => SymbolKind::Class,
        6 => SymbolKind::Method,
        7 => SymbolKind::Field,
        8 => SymbolKind::Constructor,
        9 => SymbolKind::Enum,
        10 => SymbolKind::Interface,
        11 => SymbolKind::Function,
        12 => SymbolKind::Variable, // Constant
        13 => SymbolKind::Variable,
        22 => SymbolKind::EnumMember,
        25 => SymbolKind::TypeParameter,
        _ => SymbolKind::Unknown,
    }
}
