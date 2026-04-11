use std::path::PathBuf;
use std::sync::{Arc, Mutex as StdMutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc, Mutex};
use tracing::{debug, error, info, warn};

use crate::query_graph::{BatchQueryResult, ClientMessage, LipDatabase, ServerMessage};
use crate::schema::{Action, IndexingState, OwnedAnnotationEntry, OwnedRange};

use super::embedding::EmbeddingClient;
use super::journal::{Journal, JournalEntry};
use super::manifest::ManifestResponse;
use super::tier2_manager::VerificationJob;
use super::watcher::{uri_to_path, FileWatcherHandle};

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
                    let err = ServerMessage::Error {
                        message: e.to_string(),
                    };
                    let _ = write_message(&mut stream, &err).await;
                    continue;
                }
            };

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

                let workspace_root = {
                    let mut db = self.db.lock().await;
                    match action {
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

            ClientMessage::QueryWorkspaceSymbols { query, limit } => {
                let limit = limit.unwrap_or(100);
                let mut db = self.db.lock().await;
                let syms = db.workspace_symbols(&query, limit);
                ServerMessage::WorkspaceSymbolsResult { symbols: syms }
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
                    };
                };
                // Separate URIs that already have a cached embedding from those
                // that need a network call.
                let (cached_hits, texts_needed): (Vec<_>, Vec<_>) = {
                    let db = self.db.lock().await;
                    uris.iter()
                        .map(|uri| {
                            if let Some(v) = db.get_file_embedding(uri) {
                                (Some(v.clone()), None)
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
                        Err(e) => {
                            return ServerMessage::Error {
                                message: format!("embedding failed: {e}"),
                            }
                        }
                    }
                };

                // Store new vectors in db and assemble the response.
                {
                    let mut db = self.db.lock().await;
                    for (uri, vec) in miss_uris.iter().zip(new_vecs.iter()) {
                        db.set_file_embedding(uri, vec.clone());
                    }
                }

                let mut miss_iter = new_vecs.into_iter();
                let dims = {
                    let db = self.db.lock().await;
                    db.get_file_embedding(miss_uris.first().unwrap_or(&String::new()))
                        .map(|v| v.len())
                        .unwrap_or(0)
                };
                let vectors: Vec<Option<Vec<f32>>> = cached_hits
                    .into_iter()
                    .zip(texts_needed.into_iter())
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
                ServerMessage::IndexStatusResult {
                    indexed_files,
                    pending_embedding_files: pending,
                    last_updated_ms: last_ms,
                    embedding_model,
                }
            }

            ClientMessage::QueryFileStatus { uri } => {
                let db = self.db.lock().await;
                let (indexed, has_embedding, age_seconds) = db.file_status(&uri);
                ServerMessage::FileStatusResult {
                    uri,
                    indexed,
                    has_embedding,
                    age_seconds,
                }
            }

            // ── Nearest neighbour ─────────────────────────────────────────
            ClientMessage::QueryNearest { uri, top_k } => {
                let db = self.db.lock().await;
                let Some(query_vec) = db.get_file_embedding(&uri).cloned() else {
                    return ServerMessage::Error {
                        message: format!("no embedding for {uri} — call EmbeddingBatch first"),
                    };
                };
                let results = db.nearest_by_vector(&query_vec, top_k, Some(uri.as_str()));
                ServerMessage::NearestResult { results }
            }

            ClientMessage::QueryNearestByText { text, top_k, model } => {
                let Some(client) = self.embedding_client.as_ref().as_ref() else {
                    return ServerMessage::Error {
                        message: "embedding not configured — set LIP_EMBEDDING_URL".into(),
                    };
                };
                let texts = vec![text];
                let (mut vecs, _) = match client.embed_texts(&texts, model.as_deref()).await {
                    Ok(r) => r,
                    Err(e) => {
                        return ServerMessage::Error {
                            message: format!("embedding failed: {e}"),
                        }
                    }
                };
                let query_vec = vecs.pop().unwrap_or_default();
                let db = self.db.lock().await;
                let results = db.nearest_by_vector(&query_vec, top_k, None);
                ServerMessage::NearestResult { results }
            }
        }
    }
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

        ClientMessage::QueryWorkspaceSymbols { query, limit } => {
            let limit = limit.unwrap_or(100);
            let syms = db.workspace_symbols(&query, limit);
            ok(ServerMessage::WorkspaceSymbolsResult { symbols: syms })
        }

        ClientMessage::QueryDocumentSymbols { uri } => {
            let symbols = db.file_symbols(&uri).to_vec();
            ok(ServerMessage::DocumentSymbolsResult { symbols })
        }

        ClientMessage::QueryDeadSymbols { limit } => {
            let symbols = db.dead_symbols(limit);
            ok(ServerMessage::DeadSymbolsResult { symbols })
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
            ok(ServerMessage::IndexStatusResult {
                indexed_files,
                pending_embedding_files: pending,
                last_updated_ms: last_ms,
                embedding_model: None, // no client reference available in sync context
            })
        }

        ClientMessage::QueryFileStatus { uri } => {
            let (indexed, has_embedding, age_seconds) = db.file_status(&uri);
            ok(ServerMessage::FileStatusResult {
                uri,
                indexed,
                has_embedding,
                age_seconds,
            })
        }

        // Nearest queries need an embedding vector or an async HTTP call.
        ClientMessage::QueryNearest { .. } => {
            err("QueryNearest is not permitted inside a BatchQuery")
        }
        ClientMessage::QueryNearestByText { .. } => {
            err("QueryNearestByText is not permitted inside a BatchQuery")
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
            ServerMessage::Error { message } => assert_eq!(message, "hello framing"),
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[tokio::test]
    async fn framing_large_payload() {
        let payload = "x".repeat(65_536);
        let msg = ServerMessage::Error {
            message: payload.clone(),
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
            ServerMessage::Error { message } => assert_eq!(message, payload),
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
                };
                write_message(&mut a, &msg).await.unwrap();
            }
        });

        let mut b = b;
        for i in 0u32..5 {
            let bytes = read_message(&mut b).await.unwrap();
            let decoded: ServerMessage = serde_json::from_slice(&bytes).unwrap();
            match decoded {
                ServerMessage::Error { message } => assert_eq!(message, i.to_string()),
                other => panic!("unexpected variant: {other:?}"),
            }
        }
        write_task.await.unwrap();
    }
}
