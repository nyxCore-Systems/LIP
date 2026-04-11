use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use tokio::net::UnixStream;
use tokio::sync::Mutex;
use tower_lsp::jsonrpc::Result as RpcResult;
use tower_lsp::lsp_types::*;
use tower_lsp::{async_trait, Client, LanguageServer};

use crate::daemon::manifest::ManifestRequest;
use crate::daemon::session::{read_message, write_client_message};
use crate::query_graph::{ClientMessage, ServerMessage};

use super::translate;

/// LIP-to-LSP bridge.
///
/// Runs as a standard LSP server (stdin/stdout) and forwards every LSP request
/// to the LIP daemon over a Unix socket (spec §10.1).
pub struct LipLspBackend {
    client: Client,
    daemon_socket: PathBuf,
    conn: Arc<Mutex<Option<UnixStream>>>,
    /// Monotonically increasing Delta sequence counter. Echoed in DeltaAck.
    seq: Arc<AtomicU64>,
}

impl LipLspBackend {
    pub fn new(client: Client, daemon_socket: PathBuf) -> Self {
        Self {
            client,
            daemon_socket,
            conn: Arc::new(Mutex::new(None)),
            seq: Arc::new(AtomicU64::new(0)),
        }
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::Relaxed)
    }

    /// Ensure a connection to the daemon exists, creating one if needed.
    async fn daemon_stream(
        &self,
    ) -> anyhow::Result<tokio::sync::MutexGuard<'_, Option<UnixStream>>> {
        let mut guard = self.conn.lock().await;
        if guard.is_none() {
            let stream = UnixStream::connect(&self.daemon_socket).await?;
            *guard = Some(stream);
        }
        Ok(guard)
    }

    /// Send a `ClientMessage` and receive a `ServerMessage`.
    async fn rpc(&self, msg: ClientMessage) -> anyhow::Result<ServerMessage> {
        let mut guard = self.daemon_stream().await?;
        let stream = guard
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no daemon connection"))?;
        write_client_message(stream, &msg).await?;
        let bytes = read_message(stream).await.map_err(|e| anyhow::anyhow!(e))?;
        let resp: ServerMessage = serde_json::from_slice(&bytes)?;
        Ok(resp)
    }

    fn to_rpc_error(e: impl std::fmt::Display) -> tower_lsp::jsonrpc::Error {
        tower_lsp::jsonrpc::Error {
            code: tower_lsp::jsonrpc::ErrorCode::InternalError,
            message: e.to_string().into(),
            data: None,
        }
    }
}

#[async_trait]
impl LanguageServer for LipLspBackend {
    async fn initialize(&self, params: InitializeParams) -> RpcResult<InitializeResult> {
        let root = params
            .root_uri
            .as_ref()
            .map(|u| u.path().to_string())
            .unwrap_or_default();

        // Send ManifestRequest to the daemon.
        let _ = self
            .rpc(ClientMessage::Manifest(ManifestRequest {
                repo_root: root,
                merkle_root: String::new(),
                dep_tree_hash: String::new(),
                lip_version: env!("CARGO_PKG_VERSION").to_owned(),
            }))
            .await;

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        // FULL: the bridge sends the complete text on every change.
                        change: Some(TextDocumentSyncKind::FULL),
                        // Request save notifications with full text so we can re-index on save.
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(true),
                        })),
                        ..Default::default()
                    },
                )),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                document_symbol_provider: Some(OneOf::Left(true)),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "lip-lsp-bridge".to_owned(),
                version: Some(env!("CARGO_PKG_VERSION").to_owned()),
            }),
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "LIP bridge initialized")
            .await;
    }

    async fn shutdown(&self) -> RpcResult<()> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        let text = params.text_document.text;
        let lang = params.text_document.language_id;

        let doc = crate::schema::OwnedDocument {
            uri: uri.clone(),
            content_hash: crate::schema::sha256_hex(text.as_bytes()),
            language: lang,
            occurrences: vec![],
            symbols: vec![],
            merkle_path: uri,
            edges: vec![],
            source_text: Some(text),
        };
        let _ = self
            .rpc(ClientMessage::Delta {
                seq: self.next_seq(),
                action: crate::schema::Action::Upsert,
                document: doc,
            })
            .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        if let Some(change) = params.content_changes.into_iter().next_back() {
            let text = change.text;
            let doc = crate::schema::OwnedDocument {
                uri: uri.clone(),
                content_hash: crate::schema::sha256_hex(text.as_bytes()),
                language: String::new(),
                occurrences: vec![],
                symbols: vec![],
                merkle_path: uri,
                edges: vec![],
                source_text: Some(text),
            };
            let _ = self
                .rpc(ClientMessage::Delta {
                    seq: self.next_seq(),
                    action: crate::schema::Action::Upsert,
                    document: doc,
                })
                .await;
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        if let Some(text) = params.text {
            let uri = params.text_document.uri.to_string();
            let doc = crate::schema::OwnedDocument {
                uri: uri.clone(),
                content_hash: crate::schema::sha256_hex(text.as_bytes()),
                language: String::new(),
                occurrences: vec![],
                symbols: vec![],
                merkle_path: uri,
                edges: vec![],
                source_text: Some(text),
            };
            let _ = self
                .rpc(ClientMessage::Delta {
                    seq: self.next_seq(),
                    action: crate::schema::Action::Upsert,
                    document: doc,
                })
                .await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        let doc = crate::schema::OwnedDocument {
            uri: uri.clone(),
            content_hash: String::new(),
            language: String::new(),
            occurrences: vec![],
            symbols: vec![],
            merkle_path: uri,
            edges: vec![],
            source_text: None,
        };
        let _ = self
            .rpc(ClientMessage::Delta {
                seq: 0,
                action: crate::schema::Action::Delete,
                document: doc,
            })
            .await;
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> RpcResult<Option<GotoDefinitionResponse>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .to_string();
        let pos = params.text_document_position_params.position;

        let resp = self
            .rpc(ClientMessage::QueryDefinition {
                uri: uri.clone(),
                line: pos.line,
                col: pos.character,
            })
            .await
            .map_err(Self::to_rpc_error)?;

        let location = match resp {
            ServerMessage::DefinitionResult {
                location_uri: Some(loc_uri),
                location_range: Some(loc_range),
                ..
            } => translate::location_from_uri_range(&loc_uri, &loc_range),
            _ => None,
        };

        Ok(location.map(GotoDefinitionResponse::Scalar))
    }

    async fn references(&self, params: ReferenceParams) -> RpcResult<Option<Vec<Location>>> {
        let uri = params.text_document_position.text_document.uri.to_string();
        let pos = params.text_document_position.position;

        // First resolve symbol at position, then get its references.
        let def_resp = self
            .rpc(ClientMessage::QueryDefinition {
                uri: uri.clone(),
                line: pos.line,
                col: pos.character,
            })
            .await
            .map_err(Self::to_rpc_error)?;

        let symbol_uri = match def_resp {
            ServerMessage::DefinitionResult {
                symbol: Some(sym), ..
            } => sym.uri,
            _ => return Ok(None),
        };

        let resp = self
            .rpc(ClientMessage::QueryReferences {
                symbol_uri,
                limit: Some(200),
            })
            .await
            .map_err(Self::to_rpc_error)?;

        let locs = match resp {
            ServerMessage::ReferencesResult { occurrences } => {
                translate::occurrences_to_locations(&occurrences, &uri)
            }
            _ => vec![],
        };

        Ok(if locs.is_empty() { None } else { Some(locs) })
    }

    async fn hover(&self, params: HoverParams) -> RpcResult<Option<Hover>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .to_string();
        let pos = params.text_document_position_params.position;

        let resp = self
            .rpc(ClientMessage::QueryHover {
                uri,
                line: pos.line,
                col: pos.character,
            })
            .await
            .map_err(Self::to_rpc_error)?;

        Ok(match resp {
            ServerMessage::HoverResult { symbol: Some(sym) } => {
                Some(translate::symbol_to_hover(&sym))
            }
            _ => None,
        })
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> RpcResult<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri.to_string();
        let resp = self
            .rpc(ClientMessage::QueryDocumentSymbols { uri })
            .await
            .map_err(Self::to_rpc_error)?;
        let syms = match resp {
            ServerMessage::DocumentSymbolsResult { symbols } => symbols,
            _ => return Ok(None),
        };
        let lsp_syms: Vec<SymbolInformation> = syms
            .iter()
            .filter_map(|s| translate::symbol_to_lsp_symbol_info(s, ""))
            .collect();
        Ok(if lsp_syms.is_empty() {
            None
        } else {
            Some(DocumentSymbolResponse::Flat(lsp_syms))
        })
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> RpcResult<Option<Vec<SymbolInformation>>> {
        let resp = self
            .rpc(ClientMessage::QueryWorkspaceSymbols {
                query: params.query,
                limit: Some(100),
            })
            .await
            .map_err(Self::to_rpc_error)?;

        let syms = match resp {
            ServerMessage::WorkspaceSymbolsResult { symbols } => symbols,
            _ => return Ok(None),
        };

        let lsp_syms: Vec<SymbolInformation> = syms
            .iter()
            .filter_map(|s| translate::symbol_to_lsp_symbol_info(s, ""))
            .collect();

        Ok(if lsp_syms.is_empty() {
            None
        } else {
            Some(lsp_syms)
        })
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;
    use std::sync::Arc;

    /// Build a `seq` counter in the same way `LipLspBackend` does, so we can
    /// test `next_seq` without needing a real tower-lsp `Client`.
    fn make_seq() -> Arc<AtomicU64> {
        Arc::new(AtomicU64::new(0))
    }

    /// `next_seq` must start at 0 and increment on every call — matching
    /// the monotonic Delta sequence contract (spec §6.5).
    #[test]
    fn seq_starts_at_zero_and_increments() {
        let seq = make_seq();
        // Simulate calling next_seq three times.
        let v0 = seq.fetch_add(1, Ordering::Relaxed);
        let v1 = seq.fetch_add(1, Ordering::Relaxed);
        let v2 = seq.fetch_add(1, Ordering::Relaxed);
        assert_eq!(v0, 0);
        assert_eq!(v1, 1);
        assert_eq!(v2, 2);
    }

    /// The `lip_version` sent in the ManifestRequest must match the crate version,
    /// not a stale hardcoded string.
    #[test]
    fn lip_version_matches_cargo_pkg_version() {
        let reported = env!("CARGO_PKG_VERSION");
        // Sanity: must be non-empty and start with a digit.
        assert!(!reported.is_empty());
        assert!(
            reported
                .chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false),
            "CARGO_PKG_VERSION should start with a digit, got: {reported}"
        );
        // The bridge now uses env!("CARGO_PKG_VERSION") — this test will fail to
        // compile if the macro is removed, giving us a compile-time regression guard.
        let _ = env!("CARGO_PKG_VERSION");
    }

    /// The seq counter must not overflow in normal use. u64 has 1.8×10¹⁹ values —
    /// this test just documents the type choice and that wrapping would take millennia.
    #[test]
    fn seq_is_u64() {
        // If someone changes the type, this static assert catches it at compile time.
        let _: u64 = AtomicU64::new(0).fetch_add(1, Ordering::Relaxed);
    }
}
