//! clangd Tier 2 backend for C and C++ files.
//!
//! Spawns `clangd --background-index` as a child process (requires clangd ≥ 12
//! in PATH) and communicates over the Language Server Protocol.
//!
//! A single `ClangdBackend` instance handles both C (`.c`, `.h`) and C++
//! (`.cpp`, `.cc`, `.cxx`, `.hpp`, `.hxx`) files — clangd auto-detects the
//! language from the file extension and any adjacent `compile_commands.json`.
//!
//! For each file, it:
//!
//! 1. Sends `textDocument/didOpen` with the full source.
//! 2. Calls `textDocument/documentSymbol` to obtain the symbol list.
//! 3. Calls `textDocument/hover` at each symbol's definition site to extract
//!    the type signature from the hover markdown.
//! 4. Returns a [`VerificationResult`] with confidence scores upgraded to 90.
//!
//! If `clangd` is not in PATH the backend fails to spawn and is permanently
//! disabled for the session — Tier 1 results remain fully functional.

use std::collections::HashSet;
use std::path::PathBuf;
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

pub struct ClangdBackend {
    _child: Child,
    client: LspClient,
    opened: HashSet<String>,
}

impl ClangdBackend {
    /// Spawn `clangd --background-index`, initialize the LSP session, and
    /// return a ready backend.
    ///
    /// `workspace_root` is passed as `rootUri` so clangd can locate
    /// `compile_commands.json`. Pass `None` for single-file mode.
    ///
    /// Returns `Err` if `clangd` is not found in PATH.
    pub async fn new(workspace_root: Option<PathBuf>) -> anyhow::Result<Self> {
        let mut child = Command::new("clangd")
            .args(["--background-index", "--log=error"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .context(
                "clangd not found — install clangd (https://clangd.llvm.org/installation) \
                 and ensure it is in PATH",
            )?;

        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let client = LspClient::new(stdin, stdout);

        let mut backend = Self {
            _child: child,
            client,
            opened: HashSet::new(),
        };
        backend.initialize(workspace_root).await?;
        Ok(backend)
    }

    // ── Private: LSP lifecycle ────────────────────────────────────────────────

    async fn initialize(&mut self, workspace_root: Option<PathBuf>) -> anyhow::Result<()> {
        let root_uri = workspace_root
            .as_ref()
            .map(|p| format!("file://{}", p.to_str().unwrap_or("")))
            .map(Value::String)
            .unwrap_or(Value::Null);

        let result = self
            .client
            .request(
                "initialize",
                json!({
                    "rootUri": root_uri,
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
                    }
                }),
            )
            .await?;

        info!(
            "clangd initialized (server: {:?})",
            result.get("serverInfo").and_then(|v| v.get("name"))
        );

        self.client.notify("initialized", json!({})).await?;

        // clangd needs a brief moment after initialization before it reliably
        // responds to documentSymbol requests.
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok(())
    }

    // ── Private: per-file operations ──────────────────────────────────────────

    async fn sync_file(
        &mut self,
        uri: &str,
        source: &str,
        version: i32,
        language_id: &str,
    ) -> anyhow::Result<()> {
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
                            "languageId": language_id,
                            "version":    version,
                            "text":       source
                        }
                    }),
                )
                .await?;
            self.opened.insert(uri.to_owned());
        }
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
        collect_symbols(&items, &mut out);
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

        // clangd returns hover as { contents: { kind: "markdown", value: "..." } }
        // or as a plain string for simpler responses.
        let md = result
            .pointer("/contents/value")
            .or_else(|| result.pointer("/contents"))
            .and_then(|v| v.as_str())
            .unwrap_or("");

        Ok(extract_c_signature(md))
    }

    // ── Public: full verification ─────────────────────────────────────────────

    /// Run Tier 2 verification on a single C or C++ file.
    ///
    /// `uri` must be a `file://` URI; the language ID is inferred from the
    /// file extension.
    pub async fn verify_file(
        &mut self,
        uri: &str,
        source: &str,
        version: i32,
    ) -> anyhow::Result<VerificationResult> {
        let language_id = infer_language_id(uri);
        self.sync_file(uri, source, version, language_id).await?;

        let raw = self.document_symbols(uri).await?;
        debug!("tier2(clangd): {} raw symbols for {uri}", raw.len());

        let path = uri.strip_prefix("file://").unwrap_or(uri);

        let mut symbols = Vec::with_capacity(raw.len());
        for sym in &raw {
            let sig = self
                .hover_signature(uri, sym.line, sym.col)
                .await
                .ok()
                .flatten();

            let sym_uri = format!("lip://local/{path}#{}", sym.name);

            // In C, all non-static top-level symbols are visible outside the TU.
            // At Tier 2 we trust clangd's symbol list, so mark all as exported
            // unless the name starts with an underscore (internal convention).
            let is_exported = !sym.name.starts_with('_');

            symbols.push(OwnedSymbolInfo {
                uri: sym_uri,
                display_name: sym.name.clone(),
                kind: lsp_kind_to_lip(sym.kind),
                documentation: None,
                signature: sig,
                confidence_score: 90,
                relationships: vec![],
                runtime_p99_ms: None,
                call_rate_per_s: None,
                taint_labels: vec![],
                blast_radius: 0,
                is_exported,
            });
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
}

/// Recursively collect symbols from `DocumentSymbol[]` (nested) or
/// `SymbolInformation[]` (flat). clangd returns the hierarchical form when
/// `hierarchicalDocumentSymbolSupport` is true.
fn collect_symbols(items: &[Value], out: &mut Vec<RawSymbol>) {
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

        out.push(RawSymbol {
            name,
            kind,
            line,
            col,
        });

        if let Some(Value::Array(children)) = item.get("children") {
            collect_symbols(children, out);
        }
    }
}

/// Extract a C/C++ type signature from clangd hover markdown.
///
/// clangd wraps the declaration in a fenced `cpp` or `c` code block.
/// Falls back to the first non-empty line when no fenced block is present.
fn extract_c_signature(md: &str) -> Option<String> {
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
/// C/C++-relevant LSP kinds: 5=Class, 6=Method, 8=Constructor, 9=Enum,
/// 11=Function, 13=Variable, 22=EnumMember, 23=Struct.
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
        23 => SymbolKind::Class, // Struct → Class
        25 => SymbolKind::TypeParameter,
        _ => SymbolKind::Unknown,
    }
}

/// Infer the LSP `languageId` from the file URI extension.
fn infer_language_id(uri: &str) -> &'static str {
    let ext = uri.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "c" | "h" => "c",
        _ => "cpp",
    }
}
