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
//! 5. Calls `textDocument/typeDefinition` to record cross-file type relationships.
//! 6. Calls `textDocument/inlayHints` (Type kind) to capture inferred types for
//!    local variable bindings that `documentSymbol` does not expose.
//! 7. Returns a [`VerificationResult`] with confidence scores upgraded to 90.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
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

// ─── Public types ─────────────────────────────────────────────────────────────

/// Upgraded symbols and documentation produced by Tier 2 verification.
pub struct VerificationResult {
    pub uri: String,
    /// Symbols with `confidence_score = 70` and `signature` populated where
    /// rust-analyzer returned hover type information.
    pub symbols: Vec<OwnedSymbolInfo>,
}

// ─── Backend ──────────────────────────────────────────────────────────────────

pub struct RustAnalyzerBackend {
    _child: Child,
    client: LspClient,
    _workspace: PathBuf,
    opened: HashSet<String>,
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

        let stdin = child.stdin.take().context("no stdin")?;
        let stdout = child.stdout.take().context("no stdout")?;
        let client = LspClient::new(stdin, stdout);

        let mut ra = Self {
            _child: child,
            client,
            _workspace: workspace.clone(),
            opened: HashSet::new(),
        };

        ra.initialize(&workspace).await?;
        Ok(ra)
    }

    // ── Private: LSP lifecycle ────────────────────────────────────────────────

    async fn initialize(&mut self, workspace: &Path) -> anyhow::Result<()> {
        let root_uri = format!("file://{}", workspace.display());

        let result = self
            .client
            .request(
                "initialize",
                json!({
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
                }),
            )
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
                            "languageId": "rust",
                            "version":    version,
                            "text":       source
                        }
                    }),
                )
                .await?;
            self.opened.insert(uri.to_owned());
        }
        // Allow rust-analyzer to process the change before querying.
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

        Ok(extract_code_block(md))
    }

    /// Resolve the canonical definition URI of a symbol's *type* (cross-file).
    ///
    /// Returns the LIP URI of the file where the type is defined, or `None` if
    /// the type is defined in the same file (self-referential) or the server
    /// returns no result.
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

        // Response: Location | Location[] | LocationLink[] | null
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
            .filter(|def_uri| def_uri != uri) // skip self-referential type defs
            .map(|def_uri| file_uri_to_lip_uri(&def_uri)))
    }

    /// Fetch `textDocument/inlayHints` (Type kind only) for the whole file.
    ///
    /// Returns `(line, col, label)` triples for inferred type annotations on
    /// local variable bindings — information that `documentSymbol` does not
    /// expose and that SCIP indexers do not capture.
    async fn inlay_hints(
        &mut self,
        uri: &str,
        line_count: u32,
    ) -> anyhow::Result<Vec<(u32, u32, String)>> {
        let result = self
            .client
            .request(
                "textDocument/inlayHints",
                json!({
                    "textDocument": { "uri": uri },
                    "range": {
                        "start": { "line": 0,          "character": 0 },
                        "end":   { "line": line_count, "character": 0 }
                    }
                }),
            )
            .await?;

        let Value::Array(hints) = result else {
            return Ok(vec![]);
        };

        Ok(hints
            .into_iter()
            .filter_map(|h| {
                let kind = h.get("kind")?.as_u64()?;
                if kind != 1 {
                    return None; // 1 = Type, 2 = Parameter — we only want Type
                }
                let line = h.pointer("/position/line")?.as_u64()? as u32;
                let col = h.pointer("/position/character")?.as_u64()? as u32;
                // Label can be a string or an array of InlayHintLabelPart.
                let label = match h.get("label")? {
                    Value::String(s) => s.trim_start_matches(':').trim().to_owned(),
                    Value::Array(parts) => parts
                        .iter()
                        .filter_map(|p| p.get("value").and_then(|v| v.as_str()))
                        .collect::<Vec<_>>()
                        .join("")
                        .trim_start_matches(':')
                        .trim()
                        .to_owned(),
                    _ => return None,
                };
                if label.is_empty() {
                    None
                } else {
                    Some((line, col, label))
                }
            })
            .collect())
    }

    // ── Public: full verification ─────────────────────────────────────────────

    /// Run Tier 2 verification on a single file.
    ///
    /// Syncs the file to rust-analyzer, queries the symbol list, enriches each
    /// symbol with its hover signature and cross-file type relationships, then
    /// collects local variable types from inlay hints. Returns upgraded
    /// `OwnedSymbolInfo`s with `confidence_score = 90`.
    pub async fn verify_file(
        &mut self,
        uri: &str,
        source: &str,
        version: i32,
    ) -> anyhow::Result<VerificationResult> {
        self.sync_file(uri, source, version).await?;

        let raw = self.document_symbols(uri).await?;
        debug!("tier2: {} raw symbols for {uri}", raw.len());

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

            // Infer visibility from hover signature: Rust public items start with "pub".
            let is_exported = sig
                .as_deref()
                .map(|s| s.starts_with("pub"))
                .unwrap_or(false);
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
                Language::Rust,
            );
            symbols.push(info);
        }

        // Collect local variable types from inlay hints — these are bindings
        // inside function bodies that documentSymbol does not expose, and that
        // SCIP indexers do not index. Each becomes a Variable symbol whose
        // signature carries the inferred type.
        let line_count = source.lines().count() as u32;
        let hints = self.inlay_hints(uri, line_count).await.unwrap_or_default();
        let source_lines: Vec<&str> = source.lines().collect();

        for (hint_line, hint_col, label) in &hints {
            let line_idx = *hint_line as usize;
            if line_idx >= source_lines.len() {
                continue;
            }
            let line_text = source_lines[line_idx];
            // The hint sits just after the bound name; extract the identifier
            // that ends at hint_col.
            let col = (*hint_col as usize).min(line_text.len());
            let before = &line_text[..col];
            let name = before
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .next_back()
                .filter(|n| {
                    !n.is_empty()
                        && n.chars()
                            .next()
                            .is_some_and(|c| c.is_alphabetic() || c == '_')
                        && *n != "let"
                        && *n != "mut"
                        && *n != "ref"
                });
            let Some(name) = name else {
                continue;
            };
            // @line:col suffix makes the URI unique for same-name locals.
            let sym_uri = format!("lip://local/{path}#{name}@{hint_line}:{hint_col}");
            let local_sig = format!("{name}: {label}");
            let mut info = OwnedSymbolInfo {
                uri: sym_uri,
                display_name: name.to_owned(),
                kind: SymbolKind::Variable,
                documentation: None,
                signature: Some(local_sig.clone()),
                confidence_score: 90,
                relationships: vec![],
                runtime_p99_ms: None,
                call_rate_per_s: None,
                taint_labels: vec![],
                blast_radius: 0,
                is_exported: false,
                ..Default::default()
            };
            enrich_v23(&mut info, Some(&local_sig), None, Language::Rust);
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

/// Convert a `file://` URI to a LIP `lip://local/` URI.
///
/// Matches the convention used by Tier 1 extractors:
/// `file:///path/to/foo.rs` → `lip://local//path/to/foo.rs`
pub(super) fn file_uri_to_lip_uri(file_uri: &str) -> String {
    let path = file_uri.strip_prefix("file://").unwrap_or(file_uri);
    format!("lip://local/{path}")
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
    if sig.is_empty() {
        None
    } else {
        Some(sig)
    }
}

/// Map LSP `SymbolKind` integer values to LIP `SymbolKind`.
fn lsp_kind_to_lip(kind: u64) -> SymbolKind {
    match kind {
        3 => SymbolKind::Namespace,
        4 => SymbolKind::Namespace, // Package
        5 => SymbolKind::Class,
        6 => SymbolKind::Method,
        7 => SymbolKind::Field,
        8 => SymbolKind::Constructor,
        9 => SymbolKind::Enum,
        10 => SymbolKind::Interface,
        11 => SymbolKind::Function,
        12 => SymbolKind::Variable, // Constant
        13 => SymbolKind::Variable,
        14 => SymbolKind::Variable, // String
        22 => SymbolKind::EnumMember,
        25 => SymbolKind::TypeAlias,
        _ => SymbolKind::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── file_uri_to_lip_uri ───────────────────────────────────────────────────

    #[test]
    fn file_uri_absolute_path() {
        assert_eq!(
            file_uri_to_lip_uri("file:///src/main.rs"),
            "lip://local//src/main.rs"
        );
    }

    #[test]
    fn file_uri_no_scheme_passes_through() {
        // Input without file:// is kept as-is and prefixed.
        assert_eq!(
            file_uri_to_lip_uri("/src/lib.rs"),
            "lip://local//src/lib.rs"
        );
    }

    #[test]
    fn file_uri_strips_only_file_scheme() {
        // lip:// URIs (e.g. from Tier 3 slices) are not double-wrapped.
        let uri = file_uri_to_lip_uri("file:///workspace/foo.rs");
        assert!(!uri.contains("file://"), "file:// must be stripped");
        assert!(uri.starts_with("lip://local/"));
    }

    // ── extract_code_block ────────────────────────────────────────────────────

    #[test]
    fn extract_code_block_with_lang_tag() {
        let md = "```rust\npub fn foo(x: i32) -> Bar\n```\nSome docs.";
        assert_eq!(
            extract_code_block(md).as_deref(),
            Some("pub fn foo(x: i32) -> Bar")
        );
    }

    #[test]
    fn extract_code_block_without_lang_tag() {
        let md = "```\nfn plain()\n```";
        assert_eq!(extract_code_block(md).as_deref(), Some("fn plain()"));
    }

    #[test]
    fn extract_code_block_empty_fence_returns_none() {
        assert!(extract_code_block("```rust\n\n```").is_none());
    }

    #[test]
    fn extract_code_block_no_fence_returns_none() {
        assert!(extract_code_block("plain text only").is_none());
    }

    // ── lsp_kind_to_lip ───────────────────────────────────────────────────────

    #[test]
    fn kind_mapping_spot_checks() {
        assert_eq!(lsp_kind_to_lip(5), SymbolKind::Class);
        assert_eq!(lsp_kind_to_lip(11), SymbolKind::Function);
        assert_eq!(lsp_kind_to_lip(9), SymbolKind::Enum);
        assert_eq!(lsp_kind_to_lip(10), SymbolKind::Interface);
        assert_eq!(lsp_kind_to_lip(25), SymbolKind::TypeAlias);
        assert_eq!(lsp_kind_to_lip(99), SymbolKind::Unknown);
    }
}
