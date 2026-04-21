use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, Mutex};
use tracing::{debug, error, info, warn};

use crate::query_graph::{BatchQueryResult, ClientMessage, ErrorCode, LipDatabase, ServerMessage};
use crate::schema::{Action, IndexingState, OwnedAnnotationEntry, OwnedRange};

use super::embedding::{EmbedError, EmbeddingClient};
use super::journal::{Journal, JournalEntry};
use super::manifest::ManifestResponse;
use super::tier2_manager::VerificationJob;
use super::watcher::{uri_to_path, FileWatcherHandle};

/// Monotonic protocol version. Bumped only on breaking wire-format changes.
/// Clients can detect drift by comparing against this value in `HandshakeResult`.
const PROTOCOL_VERSION: u32 = 2;

/// Convert a classified [`EmbedError`] into the appropriate wire-level
/// error response. Centralises the mapping so every embedding call site
/// reports the same [`ErrorCode`] category for the same failure mode.
fn embed_error_response(e: EmbedError) -> ServerMessage {
    let code = match e {
        EmbedError::UnknownModel(_) => ErrorCode::UnknownModel,
        EmbedError::Transport(_) | EmbedError::Protocol(_) | EmbedError::Http(_) => {
            ErrorCode::Internal
        }
    };
    ServerMessage::Error {
        message: format!("embedding failed: {e}"),
        code,
    }
}

/// Per-connection session state.
pub struct Session {
    pub db: Arc<Mutex<LipDatabase>>,
    /// Channel to the background Tier 2 manager. `None` when Tier 2 is disabled.
    pub tier2_tx: Option<mpsc::Sender<VerificationJob>>,
    /// Shared write-ahead journal. `None` when persistence is disabled.
    pub journal: Option<Arc<StdMutex<Journal>>>,
    /// Handle to the filesystem watcher. `None` when watching is disabled.
    pub watcher: Option<FileWatcherHandle>,
    /// Broadcast sender for push notifications (e.g. `SymbolUpgraded`).
    /// Kept so we can subscribe receivers for newly forked sessions.
    pub notify_tx: Option<broadcast::Sender<ServerMessage>>,
    /// HTTP embedding client. `None` when `LIP_EMBEDDING_URL` is not configured.
    pub embedding_client: Arc<Option<EmbeddingClient>>,
}

impl Session {
    pub fn new(
        db: Arc<Mutex<LipDatabase>>,
        tier2_tx: Option<mpsc::Sender<VerificationJob>>,
        journal: Option<Arc<StdMutex<Journal>>>,
        watcher: Option<FileWatcherHandle>,
        notify_tx: Option<broadcast::Sender<ServerMessage>>,
        embedding_client: Arc<Option<EmbeddingClient>>,
    ) -> Self {
        Self {
            db,
            tier2_tx,
            journal,
            watcher,
            notify_tx,
            embedding_client,
        }
    }

    fn journal_write(&self, entry: JournalEntry) {
        if let Some(j) = &self.journal {
            if let Ok(mut guard) = j.lock() {
                if let Err(e) = guard.append(&entry) {
                    warn!("journal write failed: {e}");
                }
            }
        }
    }

    /// Drive the session loop for a single connected client.
    pub async fn run(self: Arc<Self>, mut stream: UnixStream) -> anyhow::Result<()> {
        info!("new client session");
        // Subscribe to push notifications for this session's lifetime.
        let mut notify_rx: Option<broadcast::Receiver<ServerMessage>> =
            self.notify_tx.as_ref().map(|tx| tx.subscribe());

        loop {
            let msg_bytes = match read_message(&mut stream).await {
                Ok(b) => b,
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    debug!("client disconnected");
                    break;
                }
                Err(e) => {
                    error!("read error: {e}");
                    break;
                }
            };

            let msg: ClientMessage = match serde_json::from_slice(&msg_bytes) {
                Ok(m) => m,
                Err(e) => {
                    warn!("parse error: {e}");
                    let err_text = e.to_string();
                    // Unknown-variant parses are recoverable: the JSON was
                    // well-formed but carried a `type` tag this daemon
                    // doesn't know. Surface it as `UnknownMessage` with the
                    // supported list so the client can fall back gracefully
                    // instead of dropping the connection.
                    let response = if err_text.contains("unknown variant") {
                        let message_type = serde_json::from_slice::<serde_json::Value>(&msg_bytes)
                            .ok()
                            .and_then(|v| {
                                v.get("type").and_then(|t| t.as_str()).map(str::to_owned)
                            });
                        ServerMessage::UnknownMessage {
                            message_type,
                            supported: ClientMessage::supported_messages(),
                        }
                    } else {
                        ServerMessage::Error {
                            message: err_text,
                            code: ErrorCode::Internal,
                        }
                    };
                    let _ = write_message(&mut stream, &response).await;
                    continue;
                }
            };

            // Streaming requests bypass the unary handle/response cycle:
            // they write N frames + an end_stream terminator directly.
            if let ClientMessage::StreamContext {
                file_uri,
                cursor_position,
                max_tokens,
                model: _,
            } = msg
            {
                if let Err(e) = self
                    .handle_stream_context(&mut stream, file_uri, cursor_position, max_tokens)
                    .await
                {
                    error!("stream_context write error: {e}");
                    break;
                }
                continue;
            }

            let response = self.handle(msg).await;
            if let Err(e) = write_message(&mut stream, &response).await {
                error!("write error: {e}");
                break;
            }

            // Drain any pending push notifications before blocking on the next read.
            if let Some(ref mut rx) = notify_rx {
                loop {
                    match rx.try_recv() {
                        Ok(notification) => {
                            if let Err(e) = write_message(&mut stream, &notification).await {
                                error!("write error (notification): {e}");
                                break;
                            }
                        }
                        Err(broadcast::error::TryRecvError::Empty) => break,
                        Err(broadcast::error::TryRecvError::Lagged(n)) => {
                            warn!("notification receiver lagged by {n} messages");
                        }
                        Err(broadcast::error::TryRecvError::Closed) => break,
                    }
                }
            }
        }
        Ok(())
    }

    fn handle(
        &self,
        msg: ClientMessage,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ServerMessage> + Send + '_>> {
        Box::pin(self.handle_inner(msg))
    }

    async fn handle_inner(&self, msg: ClientMessage) -> ServerMessage {
        match msg {
            // ── Handshake ─────────────────────────────────────────────────
            ClientMessage::Manifest(req) => {
                info!("manifest from {} v{}", req.repo_root, req.lip_version);
                let mut db = self.db.lock().await;
                let state = if req.merkle_root.is_empty() {
                    IndexingState::Cold
                } else if db.current_merkle_root() == Some(req.merkle_root.as_str()) {
                    IndexingState::WarmFull
                } else if db.file_count() > 0 {
                    IndexingState::WarmPartial
                } else {
                    IndexingState::Cold
                };
                db.set_merkle_root(req.merkle_root.clone());
                self.journal_write(JournalEntry::SetMerkleRoot {
                    root: req.merkle_root.clone(),
                });
                if !req.repo_root.is_empty() {
                    db.set_workspace_root(PathBuf::from(&req.repo_root));
                    self.journal_write(JournalEntry::SetWorkspaceRoot {
                        path: req.repo_root.clone(),
                    });
                }
                ServerMessage::ManifestResponse(ManifestResponse {
                    cached_merkle_root: req.merkle_root,
                    missing_slices: vec![],
                    indexing_state: state,
                })
            }

            // ── File update ───────────────────────────────────────────────
            ClientMessage::Delta {
                seq,
                action,
                document,
            } => {
                let uri = document.uri.clone();
                let lang = document.language.clone();
                let source_opt = document.source_text.clone();

                let has_precomputed = document.source_text.is_none()
                    && (!document.symbols.is_empty() || !document.occurrences.is_empty());
                let content_hash = document.content_hash.clone();
                let symbols = document.symbols.clone();
                let occurrences = document.occurrences.clone();
                let edges = document.edges.clone();

                let workspace_root = {
                    let mut db = self.db.lock().await;
                    match action {
                        Action::Upsert if has_precomputed => {
                            self.journal_write(JournalEntry::UpsertFilePrecomputed {
                                uri: uri.clone(),
                                language: lang.clone(),
                                content_hash: content_hash.clone(),
                                symbols: symbols.clone(),
                                occurrences: occurrences.clone(),
                                edges: edges.clone(),
                            });
                            db.upsert_file_precomputed(
                                uri.clone(),
                                lang.clone(),
                                content_hash,
                                symbols,
                                occurrences,
                                edges,
                            );
                        }
                        Action::Upsert => {
                            let text = source_opt.clone().unwrap_or_default();
                            self.journal_write(JournalEntry::UpsertFile {
                                uri: uri.clone(),
                                text: text.clone(),
                                language: lang.clone(),
                            });
                            db.upsert_file(uri.clone(), text, lang.clone());
                        }
                        Action::Delete => {
                            self.journal_write(JournalEntry::RemoveFile { uri: uri.clone() });
                            db.remove_file(&uri);
                        }
                    }
                    db.workspace_root().map(|p| p.to_owned())
                };

                // Register / deregister with the filesystem watcher so out-of-band
                // changes (git checkout, build artefacts, etc.) are caught.
                if let Some(w) = &self.watcher {
                    match action {
                        Action::Upsert => {
                            if let Some(path) = uri_to_path(&uri) {
                                w.add(uri.clone(), path);
                            }
                        }
                        Action::Delete => {
                            if let Some(path) = uri_to_path(&uri) {
                                w.remove(path);
                            }
                        }
                    }
                }

                // Enqueue Tier 2 verification for supported languages on upsert.
                // Skipped for pre-computed imports (SCIP): source_opt is None so
                // the (Some(tx), Some(source)) guard below won't fire. This is
                // intentional — SCIP emitters are authoritative; re-verifying via
                // a local language server would be redundant and may not have the
                // right project context.
                if matches!(action, Action::Upsert) {
                    let needs_tier2 = lang == "rust"
                        || uri.ends_with(".rs")
                        || lang == "typescript"
                        || uri.ends_with(".ts")
                        || uri.ends_with(".tsx")
                        || lang == "python"
                        || uri.ends_with(".py")
                        || lang == "dart"
                        || uri.ends_with(".dart");
                    if needs_tier2 {
                        if let (Some(tx), Some(source)) = (&self.tier2_tx, source_opt) {
                            let job = VerificationJob {
                                uri: uri.clone(),
                                source,
                                workspace_root,
                                version: seq as i32,
                            };
                            // try_send: non-blocking; drop job if channel full rather
                            // than blocking the session loop.
                            if let Err(e) = tx.try_send(job) {
                                debug!("tier2 channel full, dropping job for {uri}: {e}");
                            }
                        }
                    }
                }

                // Feature 4: push IndexChanged to all active sessions after an upsert.
                if matches!(action, Action::Upsert) {
                    if let Some(tx) = &self.notify_tx {
                        let indexed_files = self.db.lock().await.file_count();
                        // SendError only occurs when there are zero active receivers — benign.
                        let _ = tx.send(ServerMessage::IndexChanged {
                            indexed_files,
                            affected_uris: vec![uri.clone()],
                        });
                    }
                }

                // Spec §6.5: send DeltaAck immediately on receipt, before analysis.
                // v0.2 will stream DeltaStream on a separate framing slot after analysis.
                ServerMessage::DeltaAck {
                    seq,
                    accepted: true,
                    error: None,
                }
            }

            // ── Queries ───────────────────────────────────────────────────
            ClientMessage::QueryDefinition { uri, line, col } => {
                let mut db = self.db.lock().await;
                // Find which symbol the cursor is on, then locate its definition.
                let sym_uri = db.symbol_at_position(&uri, line as i32, col as i32);
                match sym_uri {
                    Some(ref su) => {
                        let sym = db.symbol_by_uri(su);
                        let (loc_uri, loc_range) = db
                            .symbol_definition_location(su)
                            .unwrap_or_else(|| (uri.clone(), OwnedRange::default()));
                        ServerMessage::DefinitionResult {
                            symbol: sym,
                            location_uri: Some(loc_uri),
                            location_range: Some(loc_range),
                        }
                    }
                    None => ServerMessage::DefinitionResult {
                        symbol: None,
                        location_uri: None,
                        location_range: None,
                    },
                }
            }

            ClientMessage::QueryReferences { symbol_uri, limit } => {
                let limit = limit.unwrap_or(50);
                let mut db = self.db.lock().await;
                let uris = db.tracked_uris();
                let mut occs = vec![];
                'outer: for u in &uris {
                    for occ in db.file_occurrences(u).iter() {
                        if occ.symbol_uri == symbol_uri {
                            occs.push(occ.clone());
                            if occs.len() >= limit {
                                break 'outer;
                            }
                        }
                    }
                }
                ServerMessage::ReferencesResult { occurrences: occs }
            }

            ClientMessage::QueryHover { uri, line, col } => {
                let mut db = self.db.lock().await;
                let sym_uri = db.symbol_at_position(&uri, line as i32, col as i32);
                let sym = sym_uri.as_deref().and_then(|su| db.symbol_by_uri(su));
                ServerMessage::HoverResult { symbol: sym }
            }

            ClientMessage::QueryBlastRadius { symbol_uri } => {
                let mut db = self.db.lock().await;
                let result = db.blast_radius_for(&symbol_uri);
                ServerMessage::BlastRadiusResult(result)
            }

            ClientMessage::QueryBlastRadiusBatch {
                changed_file_uris,
                min_score,
            } => {
                let mut db = self.db.lock().await;
                let (results, not_indexed_uris) =
                    db.blast_radius_batch(&changed_file_uris, min_score);
                ServerMessage::BlastRadiusBatchResult {
                    results,
                    not_indexed_uris,
                }
            }

            ClientMessage::QueryBlastRadiusSymbol {
                symbol_uri,
                min_score,
            } => {
                let mut db = self.db.lock().await;
                let result = db.blast_radius_for_symbol(&symbol_uri, min_score);
                ServerMessage::BlastRadiusSymbolResult { result }
            }

            ClientMessage::QueryOutgoingCalls { symbol_uri, depth } => {
                let db = self.db.lock().await;
                let (pairs, truncated) = db.outgoing_calls(&symbol_uri, depth);
                let edges = pairs
                    .into_iter()
                    .map(|(from_uri, to_uri)| {
                        crate::query_graph::types::OutgoingCallEdge { from_uri, to_uri }
                    })
                    .collect();
                ServerMessage::OutgoingCallsResult { edges, truncated }
            }

            ClientMessage::QueryWorkspaceSymbols {
                query,
                limit,
                kind_filter,
                scope,
                modifier_filter,
            } => {
                let limit = limit.unwrap_or(100);
                let mut db = self.db.lock().await;
                let (symbols, ranked) = db.workspace_symbols_ranked(
                    &query,
                    limit,
                    kind_filter.as_deref(),
                    scope.as_deref(),
                    modifier_filter.as_deref(),
                );
                ServerMessage::WorkspaceSymbolsResult { symbols, ranked }
            }

            ClientMessage::QueryDocumentSymbols { uri } => {
                let mut db = self.db.lock().await;
                let symbols = db.file_symbols(&uri).to_vec();
                ServerMessage::DocumentSymbolsResult { symbols }
            }

            ClientMessage::QueryDeadSymbols { limit } => {
                let mut db = self.db.lock().await;
                let symbols = db.dead_symbols(limit);
                ServerMessage::DeadSymbolsResult { symbols }
            }

            ClientMessage::QueryInvalidatedFiles {
                changed_symbol_uris,
            } => {
                let db = self.db.lock().await;
                let file_uris = db.invalidated_files_for(&changed_symbol_uris);
                ServerMessage::InvalidatedFilesResult { file_uris }
            }

            // ── Annotations ───────────────────────────────────────────────
            ClientMessage::AnnotationSet {
                symbol_uri,
                key,
                value,
                author_id,
            } => {
                let entry = OwnedAnnotationEntry {
                    symbol_uri: symbol_uri.clone(),
                    key: key.clone(),
                    value,
                    author_id,
                    confidence: 100,
                    timestamp_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0),
                    expires_ms: 0,
                };
                self.journal_write(JournalEntry::AnnotationSet {
                    entry: entry.clone(),
                });
                let mut db = self.db.lock().await;
                db.annotation_set(entry);
                ServerMessage::AnnotationAck
            }

            ClientMessage::AnnotationGet { symbol_uri, key } => {
                let db = self.db.lock().await;
                let value = db
                    .annotation_get(&symbol_uri, &key)
                    .map(|e| e.value.clone());
                ServerMessage::AnnotationValue { value }
            }

            ClientMessage::AnnotationList { symbol_uri } => {
                let db = self.db.lock().await;
                let entries = db.annotation_list(&symbol_uri);
                ServerMessage::AnnotationEntries { entries }
            }

            // ── BatchQuery ────────────────────────────────────────────────
            ClientMessage::BatchQuery { queries } => {
                let mut results = Vec::with_capacity(queries.len());
                // Acquire the db lock once for the entire batch — one
                // mutex round-trip instead of N.
                let mut db = self.db.lock().await;
                let mut annotation_writes: Vec<OwnedAnnotationEntry> = vec![];

                for q in queries {
                    let r = process_query_sync(q, &mut db, &mut annotation_writes);
                    results.push(r);
                }
                drop(db);

                // Flush journal writes and tier2 jobs for any AnnotationSets
                // collected during the batch, now that the db lock is released.
                for entry in annotation_writes {
                    self.journal_write(JournalEntry::AnnotationSet { entry });
                }

                ServerMessage::BatchQueryResponse { results }
            }

            // ── Batch (simple) ────────────────────────────────────────────
            ClientMessage::Batch { requests } => {
                if let Some(bad) = requests.iter().find(|r| !r.is_batchable()) {
                    let _ = bad; // already matched by is_batchable
                    return ServerMessage::Error {
                        message: "nested Batch not allowed".into(),
                        code: ErrorCode::InvalidRequest,
                    };
                }
                let mut results = Vec::with_capacity(requests.len());
                for req in requests {
                    let r = self.handle(req).await;
                    results.push(r);
                }
                ServerMessage::BatchResult { results }
            }

            // ── SimilarSymbols ────────────────────────────────────────────
            ClientMessage::SimilarSymbols { query, limit } => {
                let mut db = self.db.lock().await;
                let symbols = db.similar_symbols(&query, limit);
                ServerMessage::SimilarSymbolsResult { symbols }
            }

            // ── Merkle sync ───────────────────────────────────────────────
            ClientMessage::QueryStaleFiles { files } => {
                let db = self.db.lock().await;
                let stale_uris = db.stale_files(&files);
                ServerMessage::StaleFilesResult { stale_uris }
            }

            // ── Workspace annotation search ───────────────────────────────
            ClientMessage::AnnotationWorkspaceList { key_prefix } => {
                let db = self.db.lock().await;
                let entries = db.annotations_by_key_prefix(&key_prefix);
                ServerMessage::AnnotationEntries { entries }
            }

            // ── Slice mount ───────────────────────────────────────────────
            ClientMessage::LoadSlice { slice } => {
                let pkg = format!("{}/{}@{}", slice.manager, slice.package_name, slice.version);
                let count = slice.symbols.len();
                let mut db = self.db.lock().await;
                db.mount_slice(&slice);
                info!("mounted slice {pkg} ({count} symbols)");
                ServerMessage::DeltaAck {
                    seq: 0,
                    accepted: true,
                    error: None,
                }
            }

            // ── Embeddings ────────────────────────────────────────────────
            ClientMessage::EmbeddingBatch { uris, model } => {
                let Some(client) = self.embedding_client.as_ref().as_ref() else {
                    return ServerMessage::Error {
                        message: "embedding not configured — set LIP_EMBEDDING_URL".into(),
                        code: ErrorCode::EmbeddingNotConfigured,
                    };
                };
                // Separate URIs that already have a cached embedding from those
                // that need a network call.
                let (cached_hits, texts_needed): (Vec<_>, Vec<_>) = {
                    let db = self.db.lock().await;
                    uris.iter()
                        .map(|uri| {
                            // Route by URI scheme: lip:// → symbol store, file:// → file store.
                            let cached = if uri.starts_with("lip://") {
                                db.get_symbol_embedding(uri).cloned()
                            } else {
                                db.get_file_embedding(uri).cloned()
                            };
                            if let Some(v) = cached {
                                (Some(v), None)
                            } else {
                                let text = db.file_source_text(uri).unwrap_or_default();
                                (None, Some((uri.clone(), text)))
                            }
                        })
                        .unzip()
                };

                // Embed only the cache-miss texts.
                let miss_texts: Vec<String> = texts_needed
                    .iter()
                    .filter_map(|opt| opt.as_ref().map(|(_, t)| t.clone()))
                    .collect();
                let miss_uris: Vec<String> = texts_needed
                    .iter()
                    .filter_map(|opt| opt.as_ref().map(|(u, _)| u.clone()))
                    .collect();

                let (new_vecs, used_model) = if miss_texts.is_empty() {
                    (vec![], client.default_model().to_owned())
                } else {
                    match client.embed_texts(&miss_texts, model.as_deref()).await {
                        Ok(r) => r,
                        Err(e) => return embed_error_response(e),
                    }
                };

                // Store new vectors in db (routed by URI scheme) and assemble the response.
                {
                    let mut db = self.db.lock().await;
                    for (uri, vec) in miss_uris.iter().zip(new_vecs.iter()) {
                        if uri.starts_with("lip://") {
                            db.set_symbol_embedding(uri, vec.clone(), &used_model);
                        } else {
                            db.set_file_embedding(uri, vec.clone(), &used_model);
                        }
                    }
                }

                let mut miss_iter = new_vecs.into_iter();
                let dims = {
                    let db = self.db.lock().await;
                    let empty = String::new();
                    let first = miss_uris.first().unwrap_or(&empty);
                    let v = if first.starts_with("lip://") {
                        db.get_symbol_embedding(first)
                    } else {
                        db.get_file_embedding(first)
                    };
                    v.map(|v| v.len()).unwrap_or(0)
                };
                let vectors: Vec<Option<Vec<f32>>> = cached_hits
                    .into_iter()
                    .zip(texts_needed)
                    .map(|(cached, needed)| {
                        if let Some(v) = cached {
                            Some(v)
                        } else if needed.is_some() {
                            miss_iter.next()
                        } else {
                            None
                        }
                    })
                    .collect();

                let dims = dims.max(
                    vectors
                        .iter()
                        .filter_map(|v| v.as_ref())
                        .map(|v| v.len())
                        .next()
                        .unwrap_or(0),
                );

                ServerMessage::EmbeddingBatchResult {
                    vectors,
                    model: used_model,
                    dims,
                }
            }

            // ── Index / file status ───────────────────────────────────────
            ClientMessage::QueryIndexStatus => {
                let db = self.db.lock().await;
                let (indexed_files, pending, last_ms) = db.index_status();
                let embedding_model = self
                    .embedding_client
                    .as_ref()
                    .as_ref()
                    .map(|c| c.default_model().to_owned());
                let models_in_index = db.file_embedding_model_names();
                let mixed_models = models_in_index.len() > 1;
                let tier3_sources = db.tier3_sources();
                ServerMessage::IndexStatusResult {
                    indexed_files,
                    pending_embedding_files: pending,
                    last_updated_ms: last_ms,
                    embedding_model,
                    mixed_models,
                    models_in_index,
                    tier3_sources,
                }
            }

            ClientMessage::QueryFileStatus { uri } => {
                let db = self.db.lock().await;
                let (indexed, has_embedding, age_seconds) = db.file_status(&uri);
                let embedding_model = db.file_embedding_model(&uri).map(str::to_owned);
                ServerMessage::FileStatusResult {
                    uri,
                    indexed,
                    has_embedding,
                    age_seconds,
                    embedding_model,
                }
            }

            // ── Nearest neighbour ─────────────────────────────────────────
            ClientMessage::QueryNearest {
                uri,
                top_k,
                filter,
                min_score,
            } => {
                let db = self.db.lock().await;
                let Some(query_vec) = db.get_file_embedding(&uri).cloned() else {
                    return ServerMessage::Error {
                        message: format!("no embedding for {uri} — call EmbeddingBatch first"),
                        code: ErrorCode::NoEmbedding,
                    };
                };
                let results = db.nearest_by_vector(
                    &query_vec,
                    top_k,
                    Some(uri.as_str()),
                    filter.as_deref(),
                    min_score,
                );
                ServerMessage::NearestResult { results }
            }

            ClientMessage::QueryNearestByText {
                text,
                top_k,
                model,
                filter,
                min_score,
            } => {
                let Some(client) = self.embedding_client.as_ref().as_ref() else {
                    return ServerMessage::Error {
                        message: "embedding not configured — set LIP_EMBEDDING_URL".into(),
                        code: ErrorCode::EmbeddingNotConfigured,
                    };
                };
                let texts = vec![text];
                let (mut vecs, _) = match client.embed_texts(&texts, model.as_deref()).await {
                    Ok(r) => r,
                    Err(e) => return embed_error_response(e),
                };
                let query_vec = vecs.pop().unwrap_or_default();
                let db = self.db.lock().await;
                let results =
                    db.nearest_by_vector(&query_vec, top_k, None, filter.as_deref(), min_score);
                ServerMessage::NearestResult { results }
            }

            // ── Feature 1: BatchQueryNearestByText ────────────────────────
            ClientMessage::BatchQueryNearestByText {
                queries,
                top_k,
                model,
                filter,
                min_score,
            } => {
                let Some(client) = self.embedding_client.as_ref().as_ref() else {
                    return ServerMessage::Error {
                        message: "embedding not configured — set LIP_EMBEDDING_URL".into(),
                        code: ErrorCode::EmbeddingNotConfigured,
                    };
                };
                // Embed all queries in one HTTP batch call; no lock held during await.
                let (vecs, _) = match client.embed_texts(&queries, model.as_deref()).await {
                    Ok(r) => r,
                    Err(e) => return embed_error_response(e),
                };
                let db = self.db.lock().await;
                let results = vecs
                    .iter()
                    .map(|qv| db.nearest_by_vector(qv, top_k, None, filter.as_deref(), min_score))
                    .collect();
                ServerMessage::BatchNearestResult { results }
            }

            // ── Feature 2: QueryNearestBySymbol ───────────────────────────
            ClientMessage::QueryNearestBySymbol {
                symbol_uri,
                top_k,
                model,
            } => {
                let Some(client) = self.embedding_client.as_ref().as_ref() else {
                    return ServerMessage::Error {
                        message: "embedding not configured — set LIP_EMBEDDING_URL".into(),
                        code: ErrorCode::EmbeddingNotConfigured,
                    };
                };
                // Check cache — avoid re-embedding the same symbol repeatedly.
                let cached_vec: Option<Vec<f32>> = {
                    let db = self.db.lock().await;
                    db.get_symbol_embedding(&symbol_uri).cloned()
                };
                let query_vec = if let Some(v) = cached_vec {
                    v
                } else {
                    // Build embedding text from symbol metadata.
                    let embed_text = {
                        let mut db = self.db.lock().await;
                        match db.symbol_by_uri(&symbol_uri) {
                            Some(sym) => {
                                let mut parts = vec![sym.display_name.clone()];
                                if let Some(sig) = &sym.signature {
                                    parts.push(sig.clone());
                                }
                                if let Some(doc) = &sym.documentation {
                                    parts.push(doc.clone());
                                }
                                parts.join(" ")
                            }
                            None => {
                                return ServerMessage::Error {
                                    message: format!("symbol not found: {symbol_uri}"),
                                    code: ErrorCode::Internal,
                                }
                            }
                        }
                    };
                    // Embed — no db lock held during HTTP call.
                    let texts = vec![embed_text];
                    let (mut vecs, sym_model) =
                        match client.embed_texts(&texts, model.as_deref()).await {
                            Ok(r) => r,
                            Err(e) => return embed_error_response(e),
                        };
                    let v = vecs.pop().unwrap_or_default();
                    // Cache the computed vector for future calls.
                    {
                        let mut db = self.db.lock().await;
                        db.set_symbol_embedding(&symbol_uri, v.clone(), &sym_model);
                    }
                    v
                };
                let db = self.db.lock().await;
                let results =
                    db.nearest_symbol_by_vector(&query_vec, top_k, Some(symbol_uri.as_str()), None);
                ServerMessage::NearestResult { results }
            }

            // ── Feature 3: BatchAnnotationGet ─────────────────────────────
            ClientMessage::BatchAnnotationGet { uris, key } => {
                let db = self.db.lock().await;
                let entries = uris
                    .iter()
                    .map(|u| {
                        (
                            u.clone(),
                            db.annotation_get(u, &key).map(|e| e.value.clone()),
                        )
                    })
                    .collect();
                ServerMessage::BatchAnnotationResult { entries }
            }

            // ── Feature 5: Handshake ──────────────────────────────────────
            ClientMessage::Handshake { client_version } => {
                if let Some(ref v) = client_version {
                    debug!("client handshake: client_version={v}");
                }
                ServerMessage::HandshakeResult {
                    daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
                    protocol_version: PROTOCOL_VERSION,
                    supported_messages: ClientMessage::supported_messages(),
                }
            }

            // ── v1.6: ReindexFiles ────────────────────────────────────────
            ClientMessage::ReindexFiles { uris } => {
                let mut count = 0usize;
                for uri in &uris {
                    let Some(path) = uri_to_path(uri) else {
                        continue;
                    };
                    let Ok(text) = std::fs::read_to_string(&path) else {
                        warn!("ReindexFiles: could not read {}", path.display());
                        continue;
                    };
                    let lang = {
                        use crate::indexer::language::Language;
                        Language::detect(uri, "").as_str().to_owned()
                    };
                    let mut db = self.db.lock().await;
                    db.upsert_file(uri.clone(), text, lang);
                    count += 1;
                }
                debug!("ReindexFiles: re-indexed {count}/{} files", uris.len());
                ServerMessage::DeltaAck {
                    seq: 0,
                    accepted: true,
                    error: None,
                }
            }

            // ── v1.6: Similarity ──────────────────────────────────────────
            ClientMessage::Similarity { uri_a, uri_b } => {
                let db = self.db.lock().await;
                let va = if uri_a.starts_with("lip://") {
                    db.get_symbol_embedding(&uri_a).cloned()
                } else {
                    db.get_file_embedding(&uri_a).cloned()
                };
                let vb = if uri_b.starts_with("lip://") {
                    db.get_symbol_embedding(&uri_b).cloned()
                } else {
                    db.get_file_embedding(&uri_b).cloned()
                };
                let score = match (va, vb) {
                    (Some(a), Some(b)) if a.len() == b.len() => {
                        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
                        let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
                        let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
                        if na > 0.0 && nb > 0.0 {
                            Some(dot / (na * nb))
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                ServerMessage::SimilarityResult { score }
            }

            // ── v1.6: QueryExpansion ──────────────────────────────────────
            ClientMessage::QueryExpansion {
                query,
                top_k,
                model,
            } => {
                let Some(client) = self.embedding_client.as_ref().as_ref() else {
                    return ServerMessage::Error {
                        message: "embedding not configured — set LIP_EMBEDDING_URL".into(),
                        code: ErrorCode::EmbeddingNotConfigured,
                    };
                };
                let (mut vecs, actual_model) =
                    match client.embed_texts(&[query], model.as_deref()).await {
                        Ok(r) => r,
                        Err(e) => return embed_error_response(e),
                    };
                let query_vec = vecs.pop().unwrap_or_default();
                let mut db = self.db.lock().await;
                let terms = db.query_expansion_terms(&query_vec, &actual_model, top_k);
                ServerMessage::QueryExpansionResult { terms }
            }

            // ── v1.6: Cluster ─────────────────────────────────────────────
            ClientMessage::Cluster { uris, radius } => {
                let db = self.db.lock().await;
                // Collect (uri, vector) pairs, skipping any without an embedding.
                let pairs: Vec<(String, Vec<f32>)> = uris
                    .iter()
                    .filter_map(|uri| {
                        let v = if uri.starts_with("lip://") {
                            db.get_symbol_embedding(uri)
                        } else {
                            db.get_file_embedding(uri)
                        };
                        v.map(|vec| (uri.clone(), vec.clone()))
                    })
                    .collect();

                // Single-link greedy clustering: assign each URI to the first
                // existing group that has a member within `radius`, else new group.
                let mut groups: Vec<Vec<String>> = vec![];
                let mut group_vecs: Vec<Vec<Vec<f32>>> = vec![];

                for (uri, vec) in pairs {
                    let mut placed = false;
                    'groups: for (gi, members_vecs) in group_vecs.iter().enumerate() {
                        for mv in members_vecs {
                            if mv.len() != vec.len() {
                                continue;
                            }
                            let dot: f32 = vec.iter().zip(mv.iter()).map(|(a, b)| a * b).sum();
                            let na: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
                            let nb: f32 = mv.iter().map(|x| x * x).sum::<f32>().sqrt();
                            let sim = if na > 0.0 && nb > 0.0 {
                                dot / (na * nb)
                            } else {
                                0.0
                            };
                            if sim >= radius {
                                groups[gi].push(uri.clone());
                                group_vecs[gi].push(vec.clone());
                                placed = true;
                                break 'groups;
                            }
                        }
                    }
                    if !placed {
                        groups.push(vec![uri.clone()]);
                        group_vecs.push(vec![vec]);
                    }
                }
                ServerMessage::ClusterResult { groups }
            }

            // ── v1.6: ExportEmbeddings ────────────────────────────────────
            ClientMessage::ExportEmbeddings { uris } => {
                let db = self.db.lock().await;
                let embeddings = uris
                    .iter()
                    .filter_map(|uri| {
                        let v = if uri.starts_with("lip://") {
                            db.get_symbol_embedding(uri)
                        } else {
                            db.get_file_embedding(uri)
                        };
                        v.map(|vec| (uri.clone(), vec.clone()))
                    })
                    .collect();
                ServerMessage::ExportEmbeddingsResult { embeddings }
            }

            // ── v1.7: QueryNearestByContrast ──────────────────────────────
            ClientMessage::QueryNearestByContrast {
                like_uri,
                unlike_uri,
                top_k,
                filter,
                min_score,
            } => {
                let db = self.db.lock().await;
                let vlike = if like_uri.starts_with("lip://") {
                    db.get_symbol_embedding(&like_uri).cloned()
                } else {
                    db.get_file_embedding(&like_uri).cloned()
                };
                let vunlike = if unlike_uri.starts_with("lip://") {
                    db.get_symbol_embedding(&unlike_uri).cloned()
                } else {
                    db.get_file_embedding(&unlike_uri).cloned()
                };
                match (vlike, vunlike) {
                    (Some(vl), Some(vu)) if vl.len() == vu.len() => {
                        let mut contrast: Vec<f32> =
                            vl.iter().zip(vu.iter()).map(|(a, b)| a - b).collect();
                        let norm: f32 = contrast.iter().map(|x| x * x).sum::<f32>().sqrt();
                        if norm > 0.0 {
                            for x in contrast.iter_mut() {
                                *x /= norm;
                            }
                        }
                        let results = db.nearest_by_vector(
                            &contrast,
                            top_k,
                            None,
                            filter.as_deref(),
                            min_score,
                        );
                        ServerMessage::NearestResult { results }
                    }
                    _ => ServerMessage::Error {
                        message: "both URIs must have cached embeddings with matching \
                                  dimensions — call embedding_batch first"
                            .into(),
                        code: ErrorCode::NoEmbedding,
                    },
                }
            }

            // ── v1.7: QueryOutliers ───────────────────────────────────────
            ClientMessage::QueryOutliers { uris, top_k } => {
                let db = self.db.lock().await;
                let outliers = db.outliers(&uris, top_k);
                ServerMessage::OutliersResult { outliers }
            }

            // ── v1.7: QuerySemanticDrift ──────────────────────────────────
            ClientMessage::QuerySemanticDrift { uri_a, uri_b } => {
                let db = self.db.lock().await;
                let va = if uri_a.starts_with("lip://") {
                    db.get_symbol_embedding(&uri_a).cloned()
                } else {
                    db.get_file_embedding(&uri_a).cloned()
                };
                let vb = if uri_b.starts_with("lip://") {
                    db.get_symbol_embedding(&uri_b).cloned()
                } else {
                    db.get_file_embedding(&uri_b).cloned()
                };
                let distance = match (va, vb) {
                    (Some(a), Some(b)) if a.len() == b.len() => {
                        let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
                        let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
                        let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
                        if na > 0.0 && nb > 0.0 {
                            Some(1.0 - dot / (na * nb))
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                ServerMessage::SemanticDriftResult { distance }
            }

            // ── v1.7: SimilarityMatrix ────────────────────────────────────
            ClientMessage::SimilarityMatrix { uris } => {
                let db = self.db.lock().await;
                let (result_uris, matrix) = db.similarity_matrix(&uris);
                ServerMessage::SimilarityMatrixResult {
                    uris: result_uris,
                    matrix,
                }
            }

            // ── v1.7: FindSemanticCounterpart ─────────────────────────────
            ClientMessage::FindSemanticCounterpart {
                uri,
                candidates,
                top_k,
                filter,
                min_score,
            } => {
                let db = self.db.lock().await;
                let query_vec = if uri.starts_with("lip://") {
                    db.get_symbol_embedding(&uri).cloned()
                } else {
                    db.get_file_embedding(&uri).cloned()
                };
                let Some(qv) = query_vec else {
                    return ServerMessage::Error {
                        message: format!(
                            "{uri} has no cached embedding — call embedding_batch first"
                        ),
                        code: ErrorCode::NoEmbedding,
                    };
                };
                let q_norm: f32 = qv.iter().map(|x| x * x).sum::<f32>().sqrt();
                if q_norm == 0.0 {
                    return ServerMessage::NearestResult { results: vec![] };
                }
                let pat = filter.as_deref().and_then(|f| glob::Pattern::new(f).ok());
                let threshold = min_score.unwrap_or(f32::NEG_INFINITY);
                let mut scored: Vec<crate::query_graph::types::NearestItem> = candidates
                    .iter()
                    .filter(|c| match &pat {
                        None => true,
                        Some(p) => {
                            let path = c.strip_prefix("file://").unwrap_or(c);
                            if p.as_str().contains('/') {
                                p.matches(path)
                            } else {
                                let fname = path.rsplit('/').next().unwrap_or(path);
                                p.matches(fname)
                            }
                        }
                    })
                    .filter_map(|c| {
                        let cv = if c.starts_with("lip://") {
                            db.get_symbol_embedding(c)
                        } else {
                            db.get_file_embedding(c)
                        };
                        let cv = cv?;
                        if cv.len() != qv.len() {
                            return None;
                        }
                        let dot: f32 = qv.iter().zip(cv.iter()).map(|(a, b)| a * b).sum();
                        let cn: f32 = cv.iter().map(|x| x * x).sum::<f32>().sqrt();
                        if cn == 0.0 {
                            return None;
                        }
                        let score = dot / (q_norm * cn);
                        if score < threshold {
                            return None;
                        }
                        Some(crate::query_graph::types::NearestItem {
                            uri: c.clone(),
                            score,
                            embedding_model: None,
                        })
                    })
                    .collect();
                scored.sort_by(|a, b| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                scored.truncate(top_k);
                ServerMessage::NearestResult { results: scored }
            }

            // ── v1.7: QueryCoverage ───────────────────────────────────────
            ClientMessage::QueryCoverage { root } => {
                let db = self.db.lock().await;
                let (total_files, embedded_files, by_directory) = db.coverage(&root);
                let coverage_fraction = if total_files > 0 {
                    Some(embedded_files as f32 / total_files as f32)
                } else {
                    None
                };
                ServerMessage::CoverageResult {
                    root,
                    total_files,
                    embedded_files,
                    coverage_fraction,
                    by_directory,
                }
            }

            // ── v1.8: FindBoundaries ──────────────────────────────────────
            ClientMessage::FindBoundaries {
                uri,
                chunk_lines,
                threshold,
                model,
            } => {
                let Some(client) = self.embedding_client.as_ref().as_ref() else {
                    return ServerMessage::Error {
                        message: "embedding not configured — set LIP_EMBEDDING_URL".into(),
                        code: ErrorCode::EmbeddingNotConfigured,
                    };
                };
                let chunk_size = chunk_lines.max(1);
                let source = {
                    let db = self.db.lock().await;
                    db.file_source_text(&uri).unwrap_or_default()
                };
                if source.is_empty() {
                    return ServerMessage::BoundariesResult {
                        uri,
                        boundaries: vec![],
                    };
                }
                let lines: Vec<&str> = source.lines().collect();
                let chunks: Vec<String> = lines.chunks(chunk_size).map(|c| c.join("\n")).collect();
                if chunks.len() < 2 {
                    return ServerMessage::BoundariesResult {
                        uri,
                        boundaries: vec![],
                    };
                }
                let (vecs, _) = match client.embed_texts(&chunks, model.as_deref()).await {
                    Ok(r) => r,
                    Err(e) => return embed_error_response(e),
                };
                let mut boundaries = Vec::new();
                for i in 0..vecs.len().saturating_sub(1) {
                    let va = &vecs[i];
                    let vb = &vecs[i + 1];
                    if va.len() != vb.len() {
                        continue;
                    }
                    let na: f32 = va.iter().map(|x| x * x).sum::<f32>().sqrt();
                    let nb: f32 = vb.iter().map(|x| x * x).sum::<f32>().sqrt();
                    if na == 0.0 || nb == 0.0 {
                        continue;
                    }
                    let dot: f32 = va.iter().zip(vb.iter()).map(|(a, b)| a * b).sum();
                    let sim = dot / (na * nb);
                    let dist = 1.0 - sim;
                    if dist >= threshold {
                        boundaries.push(crate::query_graph::types::BoundaryRange {
                            start_line: (i * chunk_size) as u32,
                            end_line: ((i + 1) * chunk_size).saturating_sub(1) as u32,
                            shift_magnitude: dist,
                        });
                    }
                }
                ServerMessage::BoundariesResult { uri, boundaries }
            }

            // ── v1.8: SemanticDiff ────────────────────────────────────────
            ClientMessage::SemanticDiff {
                content_a,
                content_b,
                top_k,
                model,
            } => {
                let Some(client) = self.embedding_client.as_ref().as_ref() else {
                    return ServerMessage::Error {
                        message: "embedding not configured — set LIP_EMBEDDING_URL".into(),
                        code: ErrorCode::EmbeddingNotConfigured,
                    };
                };
                let (mut vecs, _) = match client
                    .embed_texts(&[content_a, content_b], model.as_deref())
                    .await
                {
                    Ok(r) => r,
                    Err(e) => return embed_error_response(e),
                };
                if vecs.len() < 2 {
                    return ServerMessage::Error {
                        message: "embedding service returned fewer vectors than expected".into(),
                        code: ErrorCode::Internal,
                    };
                }
                let vb = vecs.pop().unwrap();
                let va = vecs.pop().unwrap();
                // Drift magnitude.
                let na: f32 = va.iter().map(|x| x * x).sum::<f32>().sqrt();
                let nb: f32 = vb.iter().map(|x| x * x).sum::<f32>().sqrt();
                let distance = if na > 0.0 && nb > 0.0 && va.len() == vb.len() {
                    let dot: f32 = va.iter().zip(vb.iter()).map(|(a, b)| a * b).sum();
                    1.0 - dot / (na * nb)
                } else {
                    0.0
                };
                // Direction: normalize(new − old) → nearest files.
                let mut contrast: Vec<f32> = vb.iter().zip(va.iter()).map(|(b, a)| b - a).collect();
                let cn: f32 = contrast.iter().map(|x| x * x).sum::<f32>().sqrt();
                if cn > 0.0 {
                    for x in contrast.iter_mut() {
                        *x /= cn;
                    }
                }
                let moving_toward = {
                    let db = self.db.lock().await;
                    db.nearest_by_vector(&contrast, top_k, None, None, None)
                };
                ServerMessage::SemanticDiffResult {
                    distance,
                    moving_toward,
                }
            }

            // ── v1.8: QueryNearestInStore ─────────────────────────────────
            ClientMessage::QueryNearestInStore {
                uri,
                store,
                top_k,
                filter,
                min_score,
            } => {
                let db = self.db.lock().await;
                let qv = if uri.starts_with("lip://") {
                    db.get_symbol_embedding(&uri).cloned()
                } else {
                    db.get_file_embedding(&uri).cloned()
                };
                let Some(qv) = qv else {
                    return ServerMessage::Error {
                        message: format!(
                            "{uri} has no cached embedding — call embedding_batch first"
                        ),
                        code: ErrorCode::NoEmbedding,
                    };
                };
                let q_norm: f32 = qv.iter().map(|x| x * x).sum::<f32>().sqrt();
                if q_norm == 0.0 {
                    return ServerMessage::NearestResult { results: vec![] };
                }
                let pat = filter.as_deref().and_then(|f| glob::Pattern::new(f).ok());
                let threshold = min_score.unwrap_or(f32::NEG_INFINITY);
                let mut scored: Vec<crate::query_graph::types::NearestItem> = store
                    .iter()
                    .filter(|(su, _)| match &pat {
                        None => true,
                        Some(p) => {
                            let path = su.strip_prefix("file://").unwrap_or(su);
                            if p.as_str().contains('/') {
                                p.matches(path)
                            } else {
                                let fname = path.rsplit('/').next().unwrap_or(path);
                                p.matches(fname)
                            }
                        }
                    })
                    .filter_map(|(store_uri, sv)| {
                        if sv.len() != qv.len() {
                            return None;
                        }
                        let sn: f32 = sv.iter().map(|x| x * x).sum::<f32>().sqrt();
                        if sn == 0.0 {
                            return None;
                        }
                        let dot: f32 = qv.iter().zip(sv.iter()).map(|(a, b)| a * b).sum();
                        let score = dot / (q_norm * sn);
                        if score < threshold {
                            return None;
                        }
                        Some(crate::query_graph::types::NearestItem {
                            uri: store_uri.clone(),
                            score,
                            embedding_model: None,
                        })
                    })
                    .collect();
                scored.sort_by(|a, b| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                scored.truncate(top_k);
                ServerMessage::NearestResult { results: scored }
            }

            // ── v1.8: QueryNoveltyScore ───────────────────────────────────
            ClientMessage::QueryNoveltyScore { uris } => {
                let db = self.db.lock().await;
                let (score, per_file) = db.novelty_scores(&uris);
                ServerMessage::NoveltyScoreResult { score, per_file }
            }

            // ── v1.8: ExtractTerminology ──────────────────────────────────
            ClientMessage::ExtractTerminology { uris, top_k } => {
                let mut db = self.db.lock().await;
                let terms = db.extract_terminology(&uris, top_k);
                ServerMessage::TerminologyResult { terms }
            }

            // ── v1.8: PruneDeleted ────────────────────────────────────────
            ClientMessage::PruneDeleted => {
                let uris = self.db.lock().await.tracked_uris();
                let checked = uris.len();
                let mut removed = Vec::new();
                for uri in &uris {
                    if let Some(path) = uri_to_path(uri) {
                        if tokio::fs::metadata(&path).await.is_err() {
                            removed.push(uri.clone());
                        }
                    }
                }
                if !removed.is_empty() {
                    let mut db = self.db.lock().await;
                    for uri in &removed {
                        db.remove_file(uri);
                    }
                    if let Some(tx) = &self.notify_tx {
                        let indexed_files = db.file_count();
                        let _ = tx.send(ServerMessage::IndexChanged {
                            indexed_files,
                            affected_uris: removed.clone(),
                        });
                    }
                }
                ServerMessage::PruneDeletedResult { checked, removed }
            }

            // ── v1.9: GetCentroid ─────────────────────────────────────────
            ClientMessage::GetCentroid { uris } => {
                let db = self.db.lock().await;
                let (vector, included) = db.centroid(&uris);
                ServerMessage::CentroidResult { vector, included }
            }

            // ── v1.9: QueryStaleEmbeddings ────────────────────────────────
            ClientMessage::QueryStaleEmbeddings { root } => {
                let entries = self.db.lock().await.file_embeddings_in_root(&root);
                let mut stale = Vec::new();
                for (uri, indexed_at_ms) in entries {
                    if let Some(path) = uri_to_path(&uri) {
                        if let Ok(meta) = tokio::fs::metadata(&path).await {
                            if let Ok(mtime) = meta.modified() {
                                if let Ok(d) = mtime.duration_since(std::time::UNIX_EPOCH) {
                                    let mtime_ms = d.as_millis() as i64;
                                    if mtime_ms > indexed_at_ms {
                                        stale.push(uri);
                                    }
                                }
                            }
                        }
                    }
                }
                ServerMessage::StaleEmbeddingsResult { uris: stale }
            }

            // ── v2.0: ExplainMatch ────────────────────────────────────────
            ClientMessage::ExplainMatch {
                query,
                result_uri,
                top_k,
                chunk_lines,
                model,
            } => {
                let Some(client) = self.embedding_client.as_ref().as_ref() else {
                    return ServerMessage::Error {
                        message: "embedding not configured — set LIP_EMBEDDING_URL".into(),
                        code: ErrorCode::EmbeddingNotConfigured,
                    };
                };
                let effective_top_k = if top_k == 0 { 5 } else { top_k };
                let chunk_size = if chunk_lines == 0 { 20 } else { chunk_lines };

                // Resolve the query embedding.
                let (query_vec, query_model) = {
                    let db = self.db.lock().await;
                    if let Some(v) = db.get_file_embedding(&query) {
                        let m = db
                            .file_embedding_model(&query)
                            .unwrap_or_else(|| client.default_model())
                            .to_owned();
                        (v.clone(), m)
                    } else {
                        drop(db);
                        // Not a cached URI — treat as free-text query.
                        let texts = vec![query];
                        match client.embed_texts(&texts, model.as_deref()).await {
                            Ok((mut vecs, m)) => (vecs.pop().unwrap_or_default(), m),
                            Err(e) => return embed_error_response(e),
                        }
                    }
                };

                if query_vec.is_empty() {
                    return ServerMessage::Error {
                        message: "could not obtain query embedding".into(),
                        code: ErrorCode::Internal,
                    };
                }

                // Read source text for result_uri.
                let source = {
                    let db = self.db.lock().await;
                    db.file_source_text(&result_uri).unwrap_or_default()
                };
                if source.is_empty() {
                    return ServerMessage::ExplainMatchResult {
                        chunks: vec![],
                        query_model,
                    };
                }

                // Chunk the source.
                let lines: Vec<&str> = source.lines().collect();
                let raw_chunks: Vec<(u32, u32, String)> = lines
                    .chunks(chunk_size)
                    .enumerate()
                    .map(|(i, chunk_lines_slice)| {
                        let start = (i * chunk_size) as u32;
                        let end = (start as usize + chunk_lines_slice.len() - 1) as u32;
                        (start, end, chunk_lines_slice.join("\n"))
                    })
                    .collect();

                if raw_chunks.is_empty() {
                    return ServerMessage::ExplainMatchResult {
                        chunks: vec![],
                        query_model,
                    };
                }

                // Embed all chunks in one call.
                let chunk_texts: Vec<String> =
                    raw_chunks.iter().map(|(_, _, t)| t.clone()).collect();
                let (chunk_vecs, chunk_model) =
                    match client.embed_texts(&chunk_texts, model.as_deref()).await {
                        Ok(r) => r,
                        Err(e) => return embed_error_response(e),
                    };
                let _ = chunk_model; // we report query_model, not per-chunk model

                // Score each chunk against the query vector.
                let q_norm: f32 = query_vec.iter().map(|x| x * x).sum::<f32>().sqrt();
                let mut scored: Vec<crate::query_graph::types::ExplanationChunk> = raw_chunks
                    .into_iter()
                    .zip(chunk_vecs)
                    .filter_map(|((start_line, end_line, chunk_text), vec)| {
                        if vec.len() != query_vec.len() || q_norm == 0.0 {
                            return None;
                        }
                        let v_norm: f32 = vec.iter().map(|x| x * x).sum::<f32>().sqrt();
                        if v_norm == 0.0 {
                            return None;
                        }
                        let dot: f32 = query_vec.iter().zip(vec.iter()).map(|(a, b)| a * b).sum();
                        let score = dot / (q_norm * v_norm);
                        Some(crate::query_graph::types::ExplanationChunk {
                            start_line,
                            end_line,
                            chunk_text,
                            score,
                        })
                    })
                    .collect();

                scored.sort_by(|a, b| {
                    b.score
                        .partial_cmp(&a.score)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                scored.truncate(effective_top_k);

                ServerMessage::ExplainMatchResult {
                    chunks: scored,
                    query_model,
                }
            }

            // ── v2.1: EmbedText ─────────────────────────────────────────
            ClientMessage::EmbedText { text, model } => {
                let Some(client) = self.embedding_client.as_ref().as_ref() else {
                    return ServerMessage::Error {
                        message: "embedding not configured — set LIP_EMBEDDING_URL".into(),
                        code: ErrorCode::EmbeddingNotConfigured,
                    };
                };
                let texts = vec![text];
                match client.embed_texts(&texts, model.as_deref()).await {
                    Ok((mut vecs, used_model)) => ServerMessage::EmbedTextResult {
                        vector: vecs.pop().unwrap_or_default(),
                        embedding_model: used_model,
                    },
                    Err(e) => embed_error_response(e),
                }
            }

            // Streaming variant — caught earlier in `run`. Reached only if a
            // client embedded one inside a Batch / BatchQuery, which is not
            // supported.
            ClientMessage::StreamContext { .. } => ServerMessage::Error {
                message: "stream_context is a streaming request and cannot be \
                          batched or nested"
                    .into(),
                code: ErrorCode::InvalidRequest,
            },

            // ── v2.1: Tier 3 provenance registration ──────────────────────
            ClientMessage::RegisterTier3Source { source } => {
                let mut db = self.db.lock().await;
                // Auto-register the source's project root for URI resolution (v2.3.1).
                if !source.project_root.is_empty() {
                    db.register_project_root(&source.project_root);
                }
                db.register_tier3_source(source);
                ServerMessage::DeltaAck {
                    seq: 0,
                    accepted: true,
                    error: None,
                }
            }

            // ── v2.3.1: standalone project-root registration ──────────────
            ClientMessage::RegisterProjectRoot { root } => {
                let mut db = self.db.lock().await;
                db.register_project_root(&root);
                ServerMessage::DeltaAck {
                    seq: 0,
                    accepted: true,
                    error: None,
                }
            }

            // ── v2.2 features ─────────────────────────────────────────────
            ClientMessage::ReindexStale {
                uris,
                max_age_seconds,
            } => {
                let mut reindexed = Vec::new();
                let mut skipped = Vec::new();
                for uri in &uris {
                    let is_stale = {
                        let db = self.db.lock().await;
                        let (indexed, _, age_seconds) = db.file_status(uri);
                        !indexed || age_seconds.map(|age| age > max_age_seconds).unwrap_or(true)
                    };
                    if is_stale {
                        let Some(path) = uri_to_path(uri) else {
                            skipped.push(uri.clone());
                            continue;
                        };
                        let Ok(text) = std::fs::read_to_string(&path) else {
                            warn!("ReindexStale: could not read {}", path.display());
                            skipped.push(uri.clone());
                            continue;
                        };
                        let lang = {
                            use crate::indexer::language::Language;
                            Language::detect(uri, "").as_str().to_owned()
                        };
                        let mut db = self.db.lock().await;
                        db.upsert_file(uri.clone(), text, lang);
                        reindexed.push(uri.clone());
                    } else {
                        skipped.push(uri.clone());
                    }
                }
                debug!(
                    "ReindexStale: reindexed {}/{} files",
                    reindexed.len(),
                    uris.len()
                );
                ServerMessage::ReindexStaleResult { reindexed, skipped }
            }

            ClientMessage::BatchFileStatus { uris } => {
                let db = self.db.lock().await;
                let entries = uris
                    .into_iter()
                    .map(|uri| {
                        let (indexed, has_embedding, age_seconds) = db.file_status(&uri);
                        let embedding_model = db.file_embedding_model(&uri).map(str::to_owned);
                        crate::query_graph::types::FileStatusEntry {
                            uri,
                            indexed,
                            has_embedding,
                            age_seconds,
                            embedding_model,
                        }
                    })
                    .collect();
                ServerMessage::BatchFileStatusResult { entries }
            }

            ClientMessage::QueryAbiHash { uri } => {
                let mut db = self.db.lock().await;
                let hash = db.abi_hash(&uri);
                ServerMessage::AbiHashResult { uri, hash }
            }
        }
    }

    /// Handle a [`ClientMessage::StreamContext`] by streaming `symbol_info`
    /// frames followed by exactly one `end_stream` terminator.
    ///
    /// Frames are written one at a time with no internal buffering — the
    /// daemon's `write_message` blocks on socket back-pressure, which throttles
    /// ranking work when the client stops reading. A closed socket surfaces
    /// as `BrokenPipe` and aborts the loop cleanly.
    async fn handle_stream_context(
        &self,
        stream: &mut UnixStream,
        file_uri: String,
        cursor_position: OwnedRange,
        max_tokens: u32,
    ) -> anyhow::Result<()> {
        use crate::query_graph::types::EndStreamReason;

        // Validate cursor position. "Outside the file" = not tracked, or line
        // beyond the file's line count.
        let line_count_opt = {
            let db = self.db.lock().await;
            db.file_source_text(&file_uri)
                .map(|t| t.lines().count() as i32)
        };
        let Some(line_count) = line_count_opt else {
            // File URI is not in the daemon's index at all — distinct
            // from a cursor past EOF. CKB-side surfaces a different
            // message ("file not indexed, run a delta first") vs. the
            // cursor-coord error, so we split the reason codes here
            // instead of overloading `Error` with a magic string.
            let term = ServerMessage::EndStream {
                reason: EndStreamReason::FileNotIndexed,
                emitted: 0,
                total_candidates: 0,
                error: Some(format!("{file_uri} is not in the daemon index")),
            };
            write_message(stream, &term).await?;
            return Ok(());
        };
        if cursor_position.start_line < 0 || cursor_position.start_line >= line_count {
            let term = ServerMessage::EndStream {
                reason: EndStreamReason::CursorOutOfRange,
                emitted: 0,
                total_candidates: 0,
                error: Some(format!(
                    "cursor line {} is outside {file_uri} ({} lines)",
                    cursor_position.start_line, line_count
                )),
            };
            write_message(stream, &term).await?;
            return Ok(());
        }

        // Rank candidates relative to the cursor.
        let candidates = {
            let mut db = self.db.lock().await;
            rank_context_candidates(
                &mut db,
                &file_uri,
                cursor_position.start_line,
                cursor_position.start_char,
            )
        };
        let total_candidates = candidates.len() as u32;

        // Empty-budget short-circuit: emit terminator immediately. Per spec
        // this counts as `budget_reached` (acceptance criterion 2).
        if max_tokens == 0 {
            let term = ServerMessage::EndStream {
                reason: EndStreamReason::BudgetReached,
                emitted: 0,
                total_candidates,
                error: None,
            };
            write_message(stream, &term).await?;
            return Ok(());
        }

        let mut emitted: u32 = 0;
        let mut spent: u64 = 0;
        let mut reason = EndStreamReason::Exhausted;

        for (sym, score) in candidates {
            let cost = estimate_token_cost(&sym);
            if spent + cost as u64 > max_tokens as u64 {
                reason = EndStreamReason::BudgetReached;
                break;
            }
            let frame = ServerMessage::SymbolInfo {
                symbol_info: sym,
                relevance_score: score,
                token_cost: cost,
            };
            // BrokenPipe / EBADF aborts the walk — client closed early.
            write_message(stream, &frame).await?;
            spent += cost as u64;
            emitted += 1;
        }

        let term = ServerMessage::EndStream {
            reason,
            emitted,
            total_candidates,
            error: None,
        };
        write_message(stream, &term).await?;
        Ok(())
    }
}

/// Conservative chars÷4 + 8 token estimate per spec §2.4.
fn estimate_token_cost(sym: &crate::schema::OwnedSymbolInfo) -> u32 {
    let sig_len = sym.signature.as_deref().map(str::len).unwrap_or(0);
    let doc_len = sym.documentation.as_deref().map(str::len).unwrap_or(0);
    ((sig_len + doc_len) as u32).div_ceil(4) + 8
}

/// Rank symbols by relevance to a cursor inside `file_uri` (spec §2.3 ordering):
/// 1. The symbol the cursor is on (definition).
/// 2. Callers — symbols whose blast-radius walk reaches the target.
/// 3. Callees / references — outgoing relationships of the target.
/// 4. Related types — relationships flagged `is_type_definition`.
///
/// Within a tier, frames are ordered by descending heuristic score.
fn rank_context_candidates(
    db: &mut crate::query_graph::LipDatabase,
    file_uri: &str,
    line: i32,
    col: i32,
) -> Vec<(crate::schema::OwnedSymbolInfo, f32)> {
    use std::collections::HashSet;

    let mut out: Vec<(crate::schema::OwnedSymbolInfo, f32)> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let target_uri_opt = db.symbol_at_position(file_uri, line, col);

    // Tier 1 — direct definition.
    if let Some(ref target_uri) = target_uri_opt {
        if let Some(sym) = db.symbol_by_uri(target_uri) {
            seen.insert(sym.uri.clone());
            out.push((sym, 1.0));
        }
    }

    // Tier 2 — callers from the blast-radius CPG walk.
    if let Some(ref target_uri) = target_uri_opt {
        let blast = db.blast_radius_for(target_uri);
        let mut callers: Vec<_> = blast
            .direct_items
            .iter()
            .chain(blast.transitive_items.iter())
            .filter(|item| !item.symbol_uri.is_empty())
            .cloned()
            .collect();
        callers.sort_by_key(|item| item.distance);
        for item in callers {
            if !seen.insert(item.symbol_uri.clone()) {
                continue;
            }
            if let Some(sym) = db.symbol_by_uri(&item.symbol_uri) {
                let score = (0.9 - 0.1 * item.distance as f32).max(0.1);
                out.push((sym, score));
            }
        }
    }

    // Tier 3 + 4 — outgoing relationships (callees, then types).
    if let Some(ref target_uri) = target_uri_opt {
        if let Some(target) = db.symbol_by_uri(target_uri) {
            // Callees and plain references first.
            for rel in target
                .relationships
                .iter()
                .filter(|r| !r.is_type_definition)
                .cloned()
                .collect::<Vec<_>>()
            {
                if !seen.insert(rel.target_uri.clone()) {
                    continue;
                }
                if let Some(sym) = db.symbol_by_uri(&rel.target_uri) {
                    out.push((sym, 0.5));
                }
            }
            // Related types last.
            for rel in target
                .relationships
                .iter()
                .filter(|r| r.is_type_definition)
                .cloned()
                .collect::<Vec<_>>()
            {
                if !seen.insert(rel.target_uri.clone()) {
                    continue;
                }
                if let Some(sym) = db.symbol_by_uri(&rel.target_uri) {
                    out.push((sym, 0.4));
                }
            }
        }
    }

    out
}

// ── Batch query helper ────────────────────────────────────────────────────────

/// Process a single query synchronously, given an already-locked database.
///
/// `Manifest`, `Delta`, and nested `BatchQuery` entries return an error result.
/// `AnnotationSet` entries are committed to the db and their entries are
/// appended to `annotation_writes` for journal persistence after the lock is released.
fn process_query_sync(
    q: ClientMessage,
    db: &mut LipDatabase,
    annotation_writes: &mut Vec<OwnedAnnotationEntry>,
) -> BatchQueryResult {
    let ok = |msg: ServerMessage| BatchQueryResult {
        ok: Some(msg),
        error: None,
    };
    let err = |msg: &str| BatchQueryResult {
        ok: None,
        error: Some(msg.into()),
    };

    match q {
        // Not permitted in a batch.
        ClientMessage::Manifest(_) => err("Manifest is not permitted inside a BatchQuery"),
        ClientMessage::Delta { .. } => err("Delta is not permitted inside a BatchQuery"),
        ClientMessage::BatchQuery { .. } => err("nested BatchQuery is not supported"),
        ClientMessage::Batch { .. } => err("nested Batch is not supported"),

        // ── Queries ───────────────────────────────────────────────────────
        ClientMessage::QueryDefinition { uri, line, col } => {
            let sym_uri = db.symbol_at_position(&uri, line as i32, col as i32);
            match sym_uri {
                Some(ref su) => {
                    let sym = db.symbol_by_uri(su);
                    let (loc_uri, loc_range) = db
                        .symbol_definition_location(su)
                        .unwrap_or_else(|| (uri.clone(), OwnedRange::default()));
                    ok(ServerMessage::DefinitionResult {
                        symbol: sym,
                        location_uri: Some(loc_uri),
                        location_range: Some(loc_range),
                    })
                }
                None => ok(ServerMessage::DefinitionResult {
                    symbol: None,
                    location_uri: None,
                    location_range: None,
                }),
            }
        }

        ClientMessage::QueryReferences { symbol_uri, limit } => {
            let limit = limit.unwrap_or(50);
            let uris = db.tracked_uris();
            let mut occs = vec![];
            'outer: for u in &uris {
                for occ in db.file_occurrences(u).iter() {
                    if occ.symbol_uri == symbol_uri {
                        occs.push(occ.clone());
                        if occs.len() >= limit {
                            break 'outer;
                        }
                    }
                }
            }
            ok(ServerMessage::ReferencesResult { occurrences: occs })
        }

        ClientMessage::QueryHover { uri, line, col } => {
            let sym_uri = db.symbol_at_position(&uri, line as i32, col as i32);
            let sym = sym_uri.as_deref().and_then(|su| db.symbol_by_uri(su));
            ok(ServerMessage::HoverResult { symbol: sym })
        }

        ClientMessage::QueryBlastRadius { symbol_uri } => {
            let result = db.blast_radius_for(&symbol_uri);
            ok(ServerMessage::BlastRadiusResult(result))
        }

        ClientMessage::QueryBlastRadiusBatch {
            changed_file_uris,
            min_score,
        } => {
            let (results, not_indexed_uris) =
                db.blast_radius_batch(&changed_file_uris, min_score);
            ok(ServerMessage::BlastRadiusBatchResult {
                results,
                not_indexed_uris,
            })
        }

        ClientMessage::QueryBlastRadiusSymbol {
            symbol_uri,
            min_score,
        } => {
            let result = db.blast_radius_for_symbol(&symbol_uri, min_score);
            ok(ServerMessage::BlastRadiusSymbolResult { result })
        }

        ClientMessage::QueryOutgoingCalls { symbol_uri, depth } => {
            let (pairs, truncated) = db.outgoing_calls(&symbol_uri, depth);
            let edges = pairs
                .into_iter()
                .map(|(from_uri, to_uri)| {
                    crate::query_graph::types::OutgoingCallEdge { from_uri, to_uri }
                })
                .collect();
            ok(ServerMessage::OutgoingCallsResult { edges, truncated })
        }

        ClientMessage::QueryWorkspaceSymbols {
            query,
            limit,
            kind_filter,
            scope,
            modifier_filter,
        } => {
            let limit = limit.unwrap_or(100);
            let (symbols, ranked) = db.workspace_symbols_ranked(
                &query,
                limit,
                kind_filter.as_deref(),
                scope.as_deref(),
                modifier_filter.as_deref(),
            );
            ok(ServerMessage::WorkspaceSymbolsResult { symbols, ranked })
        }

        ClientMessage::QueryDocumentSymbols { uri } => {
            let symbols = db.file_symbols(&uri).to_vec();
            ok(ServerMessage::DocumentSymbolsResult { symbols })
        }

        ClientMessage::QueryDeadSymbols { limit } => {
            let symbols = db.dead_symbols(limit);
            ok(ServerMessage::DeadSymbolsResult { symbols })
        }

        ClientMessage::QueryInvalidatedFiles {
            changed_symbol_uris,
        } => {
            let file_uris = db.invalidated_files_for(&changed_symbol_uris);
            ok(ServerMessage::InvalidatedFilesResult { file_uris })
        }

        // ── Annotations ───────────────────────────────────────────────────
        ClientMessage::AnnotationSet {
            symbol_uri,
            key,
            value,
            author_id,
        } => {
            let entry = OwnedAnnotationEntry {
                symbol_uri: symbol_uri.clone(),
                key: key.clone(),
                value,
                author_id,
                confidence: 100,
                timestamp_ms: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as i64)
                    .unwrap_or(0),
                expires_ms: 0,
            };
            db.annotation_set(entry.clone());
            annotation_writes.push(entry);
            ok(ServerMessage::AnnotationAck)
        }

        ClientMessage::AnnotationGet { symbol_uri, key } => {
            let value = db
                .annotation_get(&symbol_uri, &key)
                .map(|e| e.value.clone());
            ok(ServerMessage::AnnotationValue { value })
        }

        ClientMessage::AnnotationList { symbol_uri } => {
            let entries = db.annotation_list(&symbol_uri);
            ok(ServerMessage::AnnotationEntries { entries })
        }

        ClientMessage::SimilarSymbols { query, limit } => {
            let symbols = db.similar_symbols(&query, limit);
            ok(ServerMessage::SimilarSymbolsResult { symbols })
        }

        ClientMessage::QueryStaleFiles { files } => {
            let stale_uris = db.stale_files(&files);
            ok(ServerMessage::StaleFilesResult { stale_uris })
        }

        ClientMessage::AnnotationWorkspaceList { key_prefix } => {
            let entries = db.annotations_by_key_prefix(&key_prefix);
            ok(ServerMessage::AnnotationEntries { entries })
        }

        // LoadSlice requires mutable db access and is not permitted in a read-only batch.
        ClientMessage::LoadSlice { .. } => err("LoadSlice is not permitted inside a BatchQuery"),

        // EmbeddingBatch needs async HTTP — not supported in sync batch context.
        ClientMessage::EmbeddingBatch { .. } => {
            err("EmbeddingBatch is not permitted inside a BatchQuery")
        }

        // Status queries are read-only and safe inside a batch.
        ClientMessage::QueryIndexStatus => {
            let (indexed_files, pending, last_ms) = db.index_status();
            let models_in_index = db.file_embedding_model_names();
            let mixed_models = models_in_index.len() > 1;
            let tier3_sources = db.tier3_sources();
            ok(ServerMessage::IndexStatusResult {
                indexed_files,
                pending_embedding_files: pending,
                last_updated_ms: last_ms,
                embedding_model: None, // no client reference available in sync context
                mixed_models,
                models_in_index,
                tier3_sources,
            })
        }

        ClientMessage::QueryFileStatus { uri } => {
            let (indexed, has_embedding, age_seconds) = db.file_status(&uri);
            let embedding_model = db.file_embedding_model(&uri).map(str::to_owned);
            ok(ServerMessage::FileStatusResult {
                uri,
                indexed,
                has_embedding,
                age_seconds,
                embedding_model,
            })
        }

        // Nearest queries need an embedding vector or an async HTTP call.
        ClientMessage::QueryNearest { .. } => {
            err("QueryNearest is not permitted inside a BatchQuery")
        }
        ClientMessage::QueryNearestByText { .. } => {
            err("QueryNearestByText is not permitted inside a BatchQuery")
        }

        // ── New v1.5 variants ─────────────────────────────────────────────

        // Handshake is trivially synchronous.
        ClientMessage::Handshake { .. } => ok(ServerMessage::HandshakeResult {
            daemon_version: env!("CARGO_PKG_VERSION").to_owned(),
            protocol_version: PROTOCOL_VERSION,
            supported_messages: ClientMessage::supported_messages(),
        }),

        // BatchAnnotationGet is a pure read — safe inside a batch.
        ClientMessage::BatchAnnotationGet { uris, key } => {
            let entries = uris
                .iter()
                .map(|u| {
                    (
                        u.clone(),
                        db.annotation_get(u, &key).map(|e| e.value.clone()),
                    )
                })
                .collect();
            ok(ServerMessage::BatchAnnotationResult { entries })
        }

        // These two require async HTTP embedding calls.
        ClientMessage::BatchQueryNearestByText { .. } => {
            err("BatchQueryNearestByText requires async HTTP; not permitted in BatchQuery")
        }
        ClientMessage::QueryNearestBySymbol { .. } => {
            err("QueryNearestBySymbol requires async HTTP; not permitted in BatchQuery")
        }

        // ── v1.6 variants ─────────────────────────────────────────────────

        // ReindexFiles requires filesystem I/O — not permitted in sync batch context.
        ClientMessage::ReindexFiles { .. } => {
            err("ReindexFiles is not permitted inside a BatchQuery")
        }

        // Similarity is a pure read — safe inside a batch.
        ClientMessage::Similarity { uri_a, uri_b } => {
            let va = if uri_a.starts_with("lip://") {
                db.get_symbol_embedding(&uri_a).cloned()
            } else {
                db.get_file_embedding(&uri_a).cloned()
            };
            let vb = if uri_b.starts_with("lip://") {
                db.get_symbol_embedding(&uri_b).cloned()
            } else {
                db.get_file_embedding(&uri_b).cloned()
            };
            let score = match (va, vb) {
                (Some(a), Some(b)) if a.len() == b.len() => {
                    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
                    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
                    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
                    if na > 0.0 && nb > 0.0 {
                        Some(dot / (na * nb))
                    } else {
                        None
                    }
                }
                _ => None,
            };
            ok(ServerMessage::SimilarityResult { score })
        }

        // QueryExpansion and Cluster require async HTTP embedding calls.
        ClientMessage::QueryExpansion { .. } => {
            err("QueryExpansion requires async HTTP; not permitted in BatchQuery")
        }
        ClientMessage::Cluster { .. } => {
            err("Cluster requires async HTTP; not permitted in BatchQuery")
        }

        // ExportEmbeddings is a pure read — safe inside a batch.
        ClientMessage::ExportEmbeddings { uris } => {
            let embeddings = uris
                .iter()
                .filter_map(|uri| {
                    let v = if uri.starts_with("lip://") {
                        db.get_symbol_embedding(uri)
                    } else {
                        db.get_file_embedding(uri)
                    };
                    v.map(|vec| (uri.clone(), vec.clone()))
                })
                .collect();
            ok(ServerMessage::ExportEmbeddingsResult { embeddings })
        }

        // ── v1.7 variants — all pure reads, safe inside a batch ───────────
        ClientMessage::QueryNearestByContrast {
            like_uri,
            unlike_uri,
            top_k,
            filter,
            min_score,
        } => {
            let vlike = if like_uri.starts_with("lip://") {
                db.get_symbol_embedding(&like_uri).cloned()
            } else {
                db.get_file_embedding(&like_uri).cloned()
            };
            let vunlike = if unlike_uri.starts_with("lip://") {
                db.get_symbol_embedding(&unlike_uri).cloned()
            } else {
                db.get_file_embedding(&unlike_uri).cloned()
            };
            match (vlike, vunlike) {
                (Some(vl), Some(vu)) if vl.len() == vu.len() => {
                    let mut contrast: Vec<f32> =
                        vl.iter().zip(vu.iter()).map(|(a, b)| a - b).collect();
                    let norm: f32 = contrast.iter().map(|x| x * x).sum::<f32>().sqrt();
                    if norm > 0.0 {
                        for x in contrast.iter_mut() {
                            *x /= norm;
                        }
                    }
                    let results =
                        db.nearest_by_vector(&contrast, top_k, None, filter.as_deref(), min_score);
                    ok(ServerMessage::NearestResult { results })
                }
                _ => err("both URIs must have cached embeddings with matching dimensions"),
            }
        }

        ClientMessage::QueryOutliers { uris, top_k } => {
            let outliers = db.outliers(&uris, top_k);
            ok(ServerMessage::OutliersResult { outliers })
        }

        ClientMessage::QuerySemanticDrift { uri_a, uri_b } => {
            let va = if uri_a.starts_with("lip://") {
                db.get_symbol_embedding(&uri_a).cloned()
            } else {
                db.get_file_embedding(&uri_a).cloned()
            };
            let vb = if uri_b.starts_with("lip://") {
                db.get_symbol_embedding(&uri_b).cloned()
            } else {
                db.get_file_embedding(&uri_b).cloned()
            };
            let distance = match (va, vb) {
                (Some(a), Some(b)) if a.len() == b.len() => {
                    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
                    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
                    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
                    if na > 0.0 && nb > 0.0 {
                        Some(1.0 - dot / (na * nb))
                    } else {
                        None
                    }
                }
                _ => None,
            };
            ok(ServerMessage::SemanticDriftResult { distance })
        }

        ClientMessage::SimilarityMatrix { uris } => {
            let (result_uris, matrix) = db.similarity_matrix(&uris);
            ok(ServerMessage::SimilarityMatrixResult {
                uris: result_uris,
                matrix,
            })
        }

        ClientMessage::FindSemanticCounterpart {
            uri,
            candidates,
            top_k,
            filter,
            min_score,
        } => {
            let query_vec = if uri.starts_with("lip://") {
                db.get_symbol_embedding(&uri).cloned()
            } else {
                db.get_file_embedding(&uri).cloned()
            };
            let Some(qv) = query_vec else {
                return err(&format!(
                    "{uri} has no cached embedding — call embedding_batch first"
                ));
            };
            let q_norm: f32 = qv.iter().map(|x| x * x).sum::<f32>().sqrt();
            if q_norm == 0.0 {
                return ok(ServerMessage::NearestResult { results: vec![] });
            }
            let pat = filter.as_deref().and_then(|f| glob::Pattern::new(f).ok());
            let threshold = min_score.unwrap_or(f32::NEG_INFINITY);
            let mut scored: Vec<crate::query_graph::types::NearestItem> = candidates
                .iter()
                .filter(|c| match &pat {
                    None => true,
                    Some(p) => {
                        let path = c.strip_prefix("file://").unwrap_or(c);
                        if p.as_str().contains('/') {
                            p.matches(path)
                        } else {
                            let fname = path.rsplit('/').next().unwrap_or(path);
                            p.matches(fname)
                        }
                    }
                })
                .filter_map(|c| {
                    let cv = if c.starts_with("lip://") {
                        db.get_symbol_embedding(c)
                    } else {
                        db.get_file_embedding(c)
                    };
                    let cv = cv?;
                    if cv.len() != qv.len() {
                        return None;
                    }
                    let dot: f32 = qv.iter().zip(cv.iter()).map(|(a, b)| a * b).sum();
                    let cn: f32 = cv.iter().map(|x| x * x).sum::<f32>().sqrt();
                    if cn == 0.0 {
                        return None;
                    }
                    let score = dot / (q_norm * cn);
                    if score < threshold {
                        return None;
                    }
                    Some(crate::query_graph::types::NearestItem {
                        uri: c.clone(),
                        score,
                        embedding_model: None,
                    })
                })
                .collect();
            scored.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            scored.truncate(top_k);
            ok(ServerMessage::NearestResult { results: scored })
        }

        ClientMessage::QueryCoverage { root } => {
            let (total_files, embedded_files, by_directory) = db.coverage(&root);
            let coverage_fraction = if total_files > 0 {
                Some(embedded_files as f32 / total_files as f32)
            } else {
                None
            };
            ok(ServerMessage::CoverageResult {
                root,
                total_files,
                embedded_files,
                coverage_fraction,
                by_directory,
            })
        }

        // ── v1.8 variants ──────────────────────────────────────────────────

        // These require async HTTP or filesystem I/O — not permitted in sync batch context.
        ClientMessage::FindBoundaries { .. } => {
            err("FindBoundaries requires async HTTP; not permitted in BatchQuery")
        }
        ClientMessage::SemanticDiff { .. } => {
            err("SemanticDiff requires async HTTP; not permitted in BatchQuery")
        }
        ClientMessage::PruneDeleted => {
            err("PruneDeleted requires filesystem I/O; not permitted in BatchQuery")
        }

        // Pure reads — safe inside a batch.
        ClientMessage::QueryNearestInStore {
            uri,
            store,
            top_k,
            filter,
            min_score,
        } => {
            let qv = if uri.starts_with("lip://") {
                db.get_symbol_embedding(&uri).cloned()
            } else {
                db.get_file_embedding(&uri).cloned()
            };
            let Some(qv) = qv else {
                return err(&format!(
                    "{uri} has no cached embedding — call embedding_batch first"
                ));
            };
            let q_norm: f32 = qv.iter().map(|x| x * x).sum::<f32>().sqrt();
            if q_norm == 0.0 {
                return ok(ServerMessage::NearestResult { results: vec![] });
            }
            let pat = filter.as_deref().and_then(|f| glob::Pattern::new(f).ok());
            let threshold = min_score.unwrap_or(f32::NEG_INFINITY);
            let mut scored: Vec<crate::query_graph::types::NearestItem> = store
                .iter()
                .filter(|(su, _)| match &pat {
                    None => true,
                    Some(p) => {
                        let path = su.strip_prefix("file://").unwrap_or(su);
                        if p.as_str().contains('/') {
                            p.matches(path)
                        } else {
                            let fname = path.rsplit('/').next().unwrap_or(path);
                            p.matches(fname)
                        }
                    }
                })
                .filter_map(|(su, sv)| {
                    if sv.len() != qv.len() {
                        return None;
                    }
                    let sn: f32 = sv.iter().map(|x| x * x).sum::<f32>().sqrt();
                    if sn == 0.0 {
                        return None;
                    }
                    let dot: f32 = qv.iter().zip(sv.iter()).map(|(a, b)| a * b).sum();
                    let score = dot / (q_norm * sn);
                    if score < threshold {
                        return None;
                    }
                    Some(crate::query_graph::types::NearestItem {
                        uri: su.clone(),
                        score,
                        embedding_model: None,
                    })
                })
                .collect();
            scored.sort_by(|a, b| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            scored.truncate(top_k);
            ok(ServerMessage::NearestResult { results: scored })
        }

        ClientMessage::QueryNoveltyScore { uris } => {
            let (score, per_file) = db.novelty_scores(&uris);
            ok(ServerMessage::NoveltyScoreResult { score, per_file })
        }

        ClientMessage::ExtractTerminology { uris, top_k } => {
            let terms = db.extract_terminology(&uris, top_k);
            ok(ServerMessage::TerminologyResult { terms })
        }

        // ── v1.9 variants ──────────────────────────────────────────────────
        ClientMessage::GetCentroid { uris } => {
            let (vector, included) = db.centroid(&uris);
            ok(ServerMessage::CentroidResult { vector, included })
        }

        ClientMessage::QueryStaleEmbeddings { .. } => {
            err("QueryStaleEmbeddings requires filesystem I/O; not permitted in BatchQuery")
        }

        ClientMessage::ExplainMatch { .. } => {
            err("ExplainMatch requires async HTTP; not permitted in BatchQuery")
        }

        ClientMessage::StreamContext { .. } => {
            err("StreamContext is a streaming request; not permitted in BatchQuery")
        }

        ClientMessage::EmbedText { .. } => {
            err("EmbedText requires async HTTP; not permitted in BatchQuery")
        }

        ClientMessage::RegisterTier3Source { .. } => {
            err("RegisterTier3Source is a mutation; not permitted in BatchQuery")
        }

        ClientMessage::RegisterProjectRoot { .. } => {
            err("RegisterProjectRoot is a mutation; not permitted in BatchQuery")
        }

        // ── v2.2: new variants ───────────────────────────────────────────────
        ClientMessage::ReindexStale { .. } => {
            err("ReindexStale requires filesystem I/O; not permitted in BatchQuery")
        }

        ClientMessage::BatchFileStatus { uris } => {
            let entries = uris
                .into_iter()
                .map(|uri| {
                    let (indexed, has_embedding, age_seconds) = db.file_status(&uri);
                    let embedding_model = db.file_embedding_model(&uri).map(str::to_owned);
                    crate::query_graph::types::FileStatusEntry {
                        uri,
                        indexed,
                        has_embedding,
                        age_seconds,
                        embedding_model,
                    }
                })
                .collect();
            ok(ServerMessage::BatchFileStatusResult { entries })
        }

        ClientMessage::QueryAbiHash { uri } => {
            let hash = db.abi_hash(&uri);
            ok(ServerMessage::AbiHashResult { uri, hash })
        }
    }
}

// ─── Framing ─────────────────────────────────────────────────────────────────

/// Serialize `msg` as a `ClientMessage` JSON and write with a 4-byte big-endian length prefix.
///
/// The daemon reads this with `read_message` and deserializes as `ClientMessage`.
pub async fn write_client_message(
    stream: &mut UnixStream,
    msg: &crate::query_graph::ClientMessage,
) -> anyhow::Result<()> {
    let body = serde_json::to_vec(msg)?;
    stream.write_all(&(body.len() as u32).to_be_bytes()).await?;
    stream.write_all(&body).await?;
    Ok(())
}

/// Read a 4-byte big-endian length prefix, then that many bytes.
pub async fn read_message(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await?;
    Ok(body)
}

/// Serialize `msg` as JSON and write with a 4-byte big-endian length prefix.
pub async fn write_message(stream: &mut UnixStream, msg: &ServerMessage) -> anyhow::Result<()> {
    let body = serde_json::to_vec(msg)?;
    stream.write_all(&(body.len() as u32).to_be_bytes()).await?;
    stream.write_all(&body).await?;
    Ok(())
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query_graph::ServerMessage;

    /// Round-trip a ServerMessage through `write_message` / `read_message`
    /// using tokio's in-memory duplex stream.
    #[tokio::test]
    async fn framing_roundtrip() {
        // tokio::io::duplex gives us two connected AsyncRead+AsyncWrite halves.
        // UnixStream::from_std wraps a std::os::unix::net::UnixStream, which
        // requires a real socket pair. Use a real socketpair instead.
        let (a, b) = tokio::net::UnixStream::pair().unwrap();
        let msg = ServerMessage::Error {
            message: "hello framing".to_owned(),
            code: ErrorCode::Internal,
        };

        // Writer task.
        let msg_clone = msg.clone();
        let write_task = tokio::spawn(async move {
            let mut a = a;
            write_message(&mut a, &msg_clone).await.unwrap();
        });

        // Reader on b.
        let mut b = b;
        let bytes = read_message(&mut b).await.unwrap();
        write_task.await.unwrap();

        let decoded: ServerMessage = serde_json::from_slice(&bytes).unwrap();
        match decoded {
            ServerMessage::Error { message, .. } => assert_eq!(message, "hello framing"),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn framing_large_payload() {
        let payload = "x".repeat(65_536);
        let msg = ServerMessage::Error {
            message: payload.clone(),
            code: ErrorCode::Internal,
        };

        let (a, b) = tokio::net::UnixStream::pair().unwrap();
        let write_task = tokio::spawn(async move {
            let mut a = a;
            write_message(&mut a, &msg).await.unwrap();
        });

        let mut b = b;
        let bytes = read_message(&mut b).await.unwrap();
        write_task.await.unwrap();

        let decoded: ServerMessage = serde_json::from_slice(&bytes).unwrap();
        match decoded {
            ServerMessage::Error { message, .. } => assert_eq!(message, payload),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn framing_multiple_sequential_messages() {
        let (a, b) = tokio::net::UnixStream::pair().unwrap();

        let write_task = tokio::spawn(async move {
            let mut a = a;
            for i in 0u32..5 {
                let msg = ServerMessage::Error {
                    message: i.to_string(),
                    code: ErrorCode::Internal,
                };
                write_message(&mut a, &msg).await.unwrap();
            }
        });

        let mut b = b;
        for i in 0u32..5 {
            let bytes = read_message(&mut b).await.unwrap();
            let decoded: ServerMessage = serde_json::from_slice(&bytes).unwrap();
            match decoded {
                ServerMessage::Error { message, .. } => assert_eq!(message, i.to_string()),
                other => panic!("unexpected variant: {other:?}"),
            }
        }
        write_task.await.unwrap();
    }
}
