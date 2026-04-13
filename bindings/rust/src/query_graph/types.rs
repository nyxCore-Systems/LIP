use crate::schema::{OwnedRange, OwnedSymbolInfo};
use serde::{Deserialize, Serialize};

/// The exported API surface of a file — the key early-cutoff node in the query graph.
///
/// Salsa compares the new value to the cached one using `Eq`. If the API surface
/// hasn't changed (e.g. a private function body was edited), propagation stops here
/// and all callers are shielded from recomputation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiSurface {
    /// Public/exported symbols only.
    pub symbols: Vec<OwnedSymbolInfo>,
    /// SHA-256 of the serialised symbol signatures — used for fast Eq.
    pub content_hash: String,
}

/// Risk classification for a blast-radius result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    #[default]
    Low,
    Medium,
    High,
}

impl std::fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Low => f.write_str("low"),
            Self::Medium => f.write_str("medium"),
            Self::High => f.write_str("high"),
        }
    }
}

/// A single file (or symbol within a file) that is transitively affected by
/// a change to a target symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactItem {
    /// File URI of the affected file (`file:///…` or `lip://…`).
    pub file_uri: String,
    /// URI of the specific symbol in that file that depends on the target.
    /// Empty when only file-level dependency graph data is available.
    pub symbol_uri: String,
    /// Distance from the target symbol in the call / dependency graph.
    /// `1` = direct caller, `2` = caller of caller, etc.
    pub distance: u32,
    /// Confidence that this dependency is real.
    /// Decreases with distance: 0.95 → 0.85 → 0.75 → 0.50 (floor).
    pub confidence: f32,
}

impl ImpactItem {
    /// Confidence schedule matching CKB's `analyzeImpact` weighting.
    pub fn confidence_at(distance: u32) -> f32 {
        match distance {
            1 => 0.95,
            2 => 0.85,
            3 => 0.75,
            _ => 0.50,
        }
    }
}

/// A single nearest-neighbor hit returned by `ServerMessage::NearestResult`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NearestItem {
    /// File URI of the nearest neighbour.
    pub uri: String,
    /// Cosine similarity in [0.0, 1.0] — higher is more similar.
    pub score: f32,
}

/// A single fuzzy-search hit returned by `ClientMessage::SimilarSymbols`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimilarSymbol {
    pub uri: String,
    pub name: String,
    pub kind: String,
    pub score: f32,
    pub doc: Option<String>,
    pub confidence: u8,
}

/// Result of `blast_radius(symbol_uri)`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BlastRadiusResult {
    pub symbol_uri: String,
    /// Number of files that directly depend on the target symbol's file.
    /// Kept for backwards compatibility; prefer `direct_items.len()`.
    pub direct_dependents: u32,
    /// Total number of transitively affected files.
    /// Kept for backwards compatibility; prefer `direct_items.len() + transitive_items.len()`.
    pub transitive_dependents: u32,
    /// All affected file URIs (direct + transitive), deduplicated.
    /// Kept for backwards compatibility; prefer `direct_items` + `transitive_items`.
    pub affected_files: Vec<String>,
    /// Direct callers / dependents (distance = 1), richly typed.
    pub direct_items: Vec<ImpactItem>,
    /// Transitive callers / dependents (distance ≥ 2), richly typed.
    pub transitive_items: Vec<ImpactItem>,
    /// `true` when BFS was cut off by the depth or node limit.
    pub truncated: bool,
    /// Composite risk level derived from caller count and spread.
    pub risk_level: RiskLevel,
}

/// Result for a single sub-query inside a [`ClientMessage::BatchQuery`].
///
/// Exactly one of `ok` / `error` is set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchQueryResult {
    /// Successful response. `None` when `error` is set.
    pub ok: Option<ServerMessage>,
    /// Human-readable error. `None` when `ok` is set.
    pub error: Option<String>,
}

/// Wire envelope for daemon → client query responses.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    ManifestResponse(crate::daemon::manifest::ManifestResponse),
    /// Acknowledgment for a `ClientMessage::Delta`. Sent immediately on receipt,
    /// before analysis completes, so the client can detect dropped messages.
    DeltaAck {
        /// Mirrors the `seq` field from the corresponding `Delta` message.
        seq: u64,
        accepted: bool,
        /// Set when `accepted` is false; describes why the delta was rejected.
        error: Option<String>,
    },
    DeltaStream {
        deltas: Vec<crate::schema::OwnedDelta>,
    },
    DefinitionResult {
        symbol: Option<OwnedSymbolInfo>,
        /// URI of the file that contains the definition occurrence.
        /// `None` when no definition was found.
        location_uri: Option<String>,
        /// Byte-offset range of the definition occurrence within `location_uri`.
        location_range: Option<OwnedRange>,
    },
    ReferencesResult {
        occurrences: Vec<crate::schema::OwnedOccurrence>,
    },
    HoverResult {
        symbol: Option<OwnedSymbolInfo>,
    },
    BlastRadiusResult(BlastRadiusResult),
    WorkspaceSymbolsResult {
        symbols: Vec<OwnedSymbolInfo>,
    },
    DocumentSymbolsResult {
        symbols: Vec<OwnedSymbolInfo>,
    },
    DeadSymbolsResult {
        symbols: Vec<OwnedSymbolInfo>,
    },
    AnnotationAck,
    AnnotationValue {
        value: Option<String>,
    },
    AnnotationEntries {
        entries: Vec<crate::schema::OwnedAnnotationEntry>,
    },
    /// Response to a [`ClientMessage::BatchQuery`]. One result per input query, in order.
    BatchQueryResponse {
        results: Vec<BatchQueryResult>,
    },
    /// Response to a [`ClientMessage::Batch`]. One `ServerMessage` per request, in order.
    BatchResult {
        results: Vec<ServerMessage>,
    },
    /// Push notification: a symbol's confidence score was raised by Tier 2 verification.
    SymbolUpgraded {
        uri: String,
        old_confidence: u8,
        new_confidence: u8,
    },
    /// Response to a [`ClientMessage::SimilarSymbols`] fuzzy search.
    SimilarSymbolsResult {
        symbols: Vec<SimilarSymbol>,
    },
    /// Response to [`ClientMessage::QueryStaleFiles`].
    ///
    /// `stale_uris` — files where the daemon's content hash differs from the
    /// client's, or that the daemon has never indexed. The client should
    /// re-send a `Delta::Upsert` for each URI in this list.
    StaleFilesResult {
        stale_uris: Vec<String>,
    },
    Error {
        message: String,
    },
    /// Response to [`ClientMessage::EmbeddingBatch`].
    ///
    /// `vectors[i]` is `None` when the file at `uris[i]` was not found in the daemon's
    /// index. Dimensions are uniform for all `Some` entries.
    EmbeddingBatchResult {
        /// One embedding vector per requested URI (None = not indexed).
        vectors: Vec<Option<Vec<f32>>>,
        /// The model that produced the vectors.
        model: String,
        /// Vector dimensionality (0 when all entries are None).
        dims: usize,
    },
    /// Response to [`ClientMessage::QueryIndexStatus`].
    IndexStatusResult {
        /// Number of files currently in the daemon's index.
        indexed_files: usize,
        /// Files that have been updated but whose embedding has not yet been computed.
        pending_embedding_files: usize,
        /// Unix timestamp (ms) of the most recent file upsert. `None` when empty.
        last_updated_ms: Option<i64>,
        /// The embedding model name, if configured.
        embedding_model: Option<String>,
    },
    /// Response to [`ClientMessage::QueryFileStatus`].
    FileStatusResult {
        uri: String,
        /// Whether the file is currently in the daemon's symbol index.
        indexed: bool,
        /// Whether an embedding vector has been computed for this file.
        has_embedding: bool,
        /// Seconds since the file was last indexed. `None` if never indexed.
        age_seconds: Option<u64>,
    },
    /// Response to [`ClientMessage::QueryNearest`] and [`ClientMessage::QueryNearestByText`]
    /// and [`ClientMessage::QueryNearestBySymbol`].
    NearestResult {
        results: Vec<NearestItem>,
    },
    /// Response to [`ClientMessage::BatchQueryNearestByText`].
    /// `results[i]` is the nearest-neighbor list for `queries[i]`.
    BatchNearestResult {
        results: Vec<Vec<NearestItem>>,
    },
    /// Response to [`ClientMessage::BatchAnnotationGet`].
    /// Map of symbol_uri → annotation value string (`None` = not found or expired).
    BatchAnnotationResult {
        entries: std::collections::HashMap<String, Option<String>>,
    },
    /// Push notification: one or more files were upserted into the daemon's index.
    /// Sent to all active sessions after a successful `Delta::Upsert`.
    IndexChanged {
        indexed_files: usize,
        affected_uris: Vec<String>,
    },
    /// Response to [`ClientMessage::Handshake`].
    HandshakeResult {
        /// Semver string of the running daemon binary.
        daemon_version: String,
        /// Monotonic integer bumped only on breaking wire-format changes.
        protocol_version: u32,
    },

    // ── CKB v1.6 features ────────────────────────────────────────────────
    /// Response to [`ClientMessage::Similarity`].
    /// `None` when either URI has no cached embedding.
    SimilarityResult {
        score: Option<f32>,
    },

    /// Response to [`ClientMessage::QueryExpansion`].
    QueryExpansionResult {
        /// Nearest-symbol display names, ordered by descending similarity.
        terms: Vec<String>,
    },

    /// Response to [`ClientMessage::Cluster`].
    ClusterResult {
        /// Each inner `Vec` is one cluster; URIs appear in exactly one cluster.
        groups: Vec<Vec<String>>,
    },

    /// Response to [`ClientMessage::ExportEmbeddings`].
    ExportEmbeddingsResult {
        /// Map of URI → embedding vector. Only URIs with a cached vector are included.
        embeddings: std::collections::HashMap<String, Vec<f32>>,
    },
}

/// Wire envelope for client → daemon messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Manifest(crate::daemon::manifest::ManifestRequest),
    Delta {
        /// Monotonically increasing client-side counter.
        /// The daemon echoes this in `DeltaAck.seq`.
        seq: u64,
        action: crate::schema::Action,
        document: crate::schema::OwnedDocument,
    },
    QueryDefinition {
        uri: String,
        line: u32,
        col: u32,
    },
    QueryReferences {
        symbol_uri: String,
        limit: Option<usize>,
    },
    QueryHover {
        uri: String,
        line: u32,
        col: u32,
    },
    QueryBlastRadius {
        symbol_uri: String,
    },
    QueryWorkspaceSymbols {
        query: String,
        limit: Option<usize>,
    },
    QueryDocumentSymbols {
        uri: String,
    },
    QueryDeadSymbols {
        limit: Option<usize>,
    },
    AnnotationSet {
        symbol_uri: String,
        key: String,
        value: String,
        author_id: String,
    },
    AnnotationGet {
        symbol_uri: String,
        key: String,
    },
    AnnotationList {
        symbol_uri: String,
    },
    /// Return all non-expired annotations whose key starts with `key_prefix`,
    /// across every tracked symbol. Pass `""` to list everything.
    AnnotationWorkspaceList {
        key_prefix: String,
    },
    /// Execute multiple queries in a single Unix socket round-trip.
    ///
    /// The daemon processes each sub-query under a single db lock acquisition and
    /// returns one [`BatchQueryResult`] per input query, preserving order.
    ///
    /// Restrictions: `Manifest`, `Delta`, and nested `BatchQuery` entries are
    /// rejected with an error entry rather than aborting the whole batch.
    BatchQuery {
        queries: Vec<ClientMessage>,
    },
    /// Simple batch: execute multiple requests and return one `ServerMessage` per
    /// request, in order. Nested `Batch` entries are rejected immediately.
    Batch {
        requests: Vec<ClientMessage>,
    },
    /// Trigram fuzzy-search across all tracked symbol names and documentation.
    SimilarSymbols {
        query: String,
        limit: usize,
    },
    /// Merkle sync probe: given the client's per-file content hashes, returns the
    /// URIs whose daemon-side hash differs or that the daemon has never seen.
    /// One round-trip on reconnect tells the client exactly which files to re-Delta.
    QueryStaleFiles {
        files: Vec<(String, String)>,
    },
    /// Load a pre-built dependency slice into the daemon's symbol graph.
    ///
    /// All symbols in the slice are merged at Tier 3 confidence (score=100).
    /// Idempotent: re-loading the same package key replaces prior symbols.
    /// Returns `DeltaAck { seq: 0, accepted: true }` on success.
    LoadSlice {
        slice: crate::schema::OwnedDependencySlice,
    },
    /// Compute (or retrieve cached) embedding vectors for a batch of file URIs.
    ///
    /// The daemon uses the HTTP embedding endpoint configured via `LIP_EMBEDDING_URL`.
    /// Already-cached embeddings are returned without a network call.
    /// Returns `EmbeddingBatchResult`.
    EmbeddingBatch {
        uris: Vec<String>,
        /// Override the model for this request. `None` uses the daemon's default.
        model: Option<String>,
    },
    /// Request overall daemon index health and embedding coverage.
    /// Returns `IndexStatusResult`.
    QueryIndexStatus,
    /// Request the indexing status of a single file.
    /// Returns `FileStatusResult`.
    QueryFileStatus {
        uri: String,
    },
    /// Find the `top_k` files whose stored embedding is most similar to the file at `uri`.
    /// The file must have an embedding (call `EmbeddingBatch` first if needed).
    /// Returns `NearestResult`.
    QueryNearest {
        uri: String,
        top_k: usize,
    },
    /// Find the `top_k` files whose stored embedding is most similar to the given text.
    /// The daemon embeds `text` on the fly and runs cosine search.
    /// Returns `NearestResult`.
    QueryNearestByText {
        text: String,
        top_k: usize,
        model: Option<String>,
    },
    /// Embed multiple query strings in one round-trip and return the top-k nearest
    /// files for each. Returns `BatchNearestResult`.
    BatchQueryNearestByText {
        queries: Vec<String>,
        top_k: usize,
        model: Option<String>,
    },
    /// Find the `top_k` symbols whose stored embedding is most similar to the given
    /// symbol. The daemon embeds the symbol's text on the fly (using display_name +
    /// signature + doc) and searches the symbol embedding store.
    /// Returns `NearestResult`.
    QueryNearestBySymbol {
        symbol_uri: String,
        top_k: usize,
        model: Option<String>,
    },
    /// Get annotations for multiple symbol URIs under a single db lock.
    /// Returns `BatchAnnotationResult`.
    BatchAnnotationGet {
        uris: Vec<String>,
        key: String,
    },
    /// Protocol version handshake. Returns `HandshakeResult`.
    /// Clients should send this immediately on connect to detect version drift.
    Handshake {
        client_version: Option<String>,
    },

    // ── CKB v1.6 features ────────────────────────────────────────────────
    /// Force a re-index of specific file URIs from disk, bypassing the directory
    /// scan. Useful when the client knows exactly which files changed out-of-band
    /// (e.g. after a selective git checkout). Returns `DeltaAck`.
    ReindexFiles {
        uris: Vec<String>,
    },

    /// Pairwise cosine similarity of two stored embeddings.
    /// Returns `SimilarityResult { score: None }` when either URI has no cached
    /// embedding — call `EmbeddingBatch` first if needed.
    Similarity {
        uri_a: String,
        uri_b: String,
    },

    /// Nearest-neighbour query-expansion: embed `query`, find the `top_k` nearest
    /// symbols, and return their display names as expansion terms.
    /// Returns `QueryExpansionResult`.
    QueryExpansion {
        query: String,
        top_k: usize,
        model: Option<String>,
    },

    /// Group `uris` by embedding proximity within `radius` (cosine distance).
    /// URIs without a cached embedding are silently excluded.
    /// Returns `ClusterResult`.
    Cluster {
        uris: Vec<String>,
        /// Cosine-similarity threshold: two URIs are in the same group when their
        /// pairwise similarity is ≥ `radius`.
        radius: f32,
    },

    /// Return the raw stored embedding vectors for `uris`.
    /// URIs with no cached embedding are omitted from the result map.
    /// Returns `ExportEmbeddingsResult`.
    ExportEmbeddings {
        uris: Vec<String>,
    },
}

impl ClientMessage {
    /// Returns `true` for any message that may appear inside a [`ClientMessage::Batch`].
    /// A `Batch` itself is excluded to prevent nesting. `LoadSlice` is also excluded
    /// because it requires mutable database access outside the read-only batch lock.
    pub fn is_batchable(&self) -> bool {
        !matches!(
            self,
            ClientMessage::Batch { .. }
                | ClientMessage::LoadSlice { .. }
                | ClientMessage::EmbeddingBatch { .. }
                | ClientMessage::BatchQueryNearestByText { .. }
                | ClientMessage::QueryNearestBySymbol { .. }
                | ClientMessage::ReindexFiles { .. }
                | ClientMessage::QueryExpansion { .. }
                | ClientMessage::Cluster { .. }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip_client(msg: &ClientMessage) -> ClientMessage {
        let json = serde_json::to_string(msg).expect("serialize");
        serde_json::from_str(&json).expect("deserialize")
    }

    fn round_trip_server(msg: &ServerMessage) -> ServerMessage {
        let json = serde_json::to_string(msg).expect("serialize");
        serde_json::from_str(&json).expect("deserialize")
    }

    #[test]
    fn batch_query_nearest_by_text_round_trips() {
        let msg = ClientMessage::BatchQueryNearestByText {
            queries: vec!["verify token".into(), "hash password".into()],
            top_k: 5,
            model: None,
        };
        let rt = round_trip_client(&msg);
        let ClientMessage::BatchQueryNearestByText { queries, top_k, .. } = rt else {
            panic!("wrong variant");
        };
        assert_eq!(queries.len(), 2);
        assert_eq!(top_k, 5);
    }

    #[test]
    fn query_nearest_by_symbol_round_trips() {
        let msg = ClientMessage::QueryNearestBySymbol {
            symbol_uri: "lip://local/src/main.rs#foo".into(),
            top_k: 3,
            model: Some("text-embedding-3-small".into()),
        };
        let rt = round_trip_client(&msg);
        let ClientMessage::QueryNearestBySymbol {
            symbol_uri, top_k, ..
        } = rt
        else {
            panic!("wrong variant");
        };
        assert_eq!(symbol_uri, "lip://local/src/main.rs#foo");
        assert_eq!(top_k, 3);
    }

    #[test]
    fn batch_annotation_get_round_trips() {
        let msg = ClientMessage::BatchAnnotationGet {
            uris: vec!["lip://local/a.rs#foo".into(), "lip://local/b.rs#bar".into()],
            key: "team:owner".into(),
        };
        let rt = round_trip_client(&msg);
        let ClientMessage::BatchAnnotationGet { uris, key } = rt else {
            panic!("wrong variant");
        };
        assert_eq!(uris.len(), 2);
        assert_eq!(key, "team:owner");
    }

    #[test]
    fn handshake_round_trips() {
        let msg = ClientMessage::Handshake {
            client_version: Some("1.5.0".into()),
        };
        let rt = round_trip_client(&msg);
        let ClientMessage::Handshake { client_version } = rt else {
            panic!("wrong variant");
        };
        assert_eq!(client_version.as_deref(), Some("1.5.0"));
    }

    #[test]
    fn handshake_result_round_trips() {
        let msg = ServerMessage::HandshakeResult {
            daemon_version: "1.5.0".into(),
            protocol_version: 1,
        };
        let rt = round_trip_server(&msg);
        let ServerMessage::HandshakeResult {
            daemon_version,
            protocol_version,
        } = rt
        else {
            panic!("wrong variant");
        };
        assert_eq!(daemon_version, "1.5.0");
        assert_eq!(protocol_version, 1);
    }

    #[test]
    fn index_changed_round_trips() {
        let msg = ServerMessage::IndexChanged {
            indexed_files: 42,
            affected_uris: vec!["file:///src/main.rs".into()],
        };
        let rt = round_trip_server(&msg);
        let ServerMessage::IndexChanged {
            indexed_files,
            affected_uris,
        } = rt
        else {
            panic!("wrong variant");
        };
        assert_eq!(indexed_files, 42);
        assert_eq!(affected_uris.len(), 1);
    }

    #[test]
    fn batch_nearest_result_round_trips() {
        let msg = ServerMessage::BatchNearestResult {
            results: vec![
                vec![NearestItem {
                    uri: "file:///a.rs".into(),
                    score: 0.9,
                }],
                vec![],
            ],
        };
        let rt = round_trip_server(&msg);
        let ServerMessage::BatchNearestResult { results } = rt else {
            panic!("wrong variant");
        };
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].len(), 1);
        assert!((results[0][0].score - 0.9).abs() < 1e-5);
    }

    #[test]
    fn batch_nearest_not_batchable() {
        let msg = ClientMessage::BatchQueryNearestByText {
            queries: vec![],
            top_k: 1,
            model: None,
        };
        assert!(!msg.is_batchable());
    }

    #[test]
    fn query_nearest_by_symbol_not_batchable() {
        let msg = ClientMessage::QueryNearestBySymbol {
            symbol_uri: "lip://x".into(),
            top_k: 1,
            model: None,
        };
        assert!(!msg.is_batchable());
    }

    #[test]
    fn handshake_is_batchable() {
        let msg = ClientMessage::Handshake {
            client_version: None,
        };
        assert!(msg.is_batchable());
    }

    #[test]
    fn batch_annotation_get_is_batchable() {
        let msg = ClientMessage::BatchAnnotationGet {
            uris: vec![],
            key: "k".into(),
        };
        assert!(msg.is_batchable());
    }

    #[test]
    fn reindex_files_round_trips() {
        let msg = ClientMessage::ReindexFiles {
            uris: vec!["file:///src/main.rs".into(), "file:///src/lib.rs".into()],
        };
        let rt = round_trip_client(&msg);
        let ClientMessage::ReindexFiles { uris } = rt else {
            panic!("wrong variant");
        };
        assert_eq!(uris.len(), 2);
    }

    #[test]
    fn reindex_files_not_batchable() {
        let msg = ClientMessage::ReindexFiles { uris: vec![] };
        assert!(!msg.is_batchable());
    }

    #[test]
    fn similarity_round_trips() {
        let msg = ClientMessage::Similarity {
            uri_a: "file:///src/a.rs".into(),
            uri_b: "file:///src/b.rs".into(),
        };
        let rt = round_trip_client(&msg);
        let ClientMessage::Similarity { uri_a, uri_b } = rt else {
            panic!("wrong variant");
        };
        assert_eq!(uri_a, "file:///src/a.rs");
        assert_eq!(uri_b, "file:///src/b.rs");
    }

    #[test]
    fn similarity_is_batchable() {
        let msg = ClientMessage::Similarity {
            uri_a: "file:///a.rs".into(),
            uri_b: "file:///b.rs".into(),
        };
        assert!(msg.is_batchable());
    }

    #[test]
    fn similarity_result_round_trips() {
        let msg = ServerMessage::SimilarityResult { score: Some(0.85) };
        let rt = round_trip_server(&msg);
        let ServerMessage::SimilarityResult { score } = rt else {
            panic!("wrong variant");
        };
        assert!((score.unwrap() - 0.85).abs() < 1e-5);
    }

    #[test]
    fn query_expansion_not_batchable() {
        let msg = ClientMessage::QueryExpansion {
            query: "auth".into(),
            top_k: 5,
            model: None,
        };
        assert!(!msg.is_batchable());
    }

    #[test]
    fn cluster_not_batchable() {
        let msg = ClientMessage::Cluster {
            uris: vec![],
            radius: 0.8,
        };
        assert!(!msg.is_batchable());
    }

    #[test]
    fn export_embeddings_round_trips() {
        let msg = ClientMessage::ExportEmbeddings {
            uris: vec!["file:///src/main.rs".into()],
        };
        let rt = round_trip_client(&msg);
        let ClientMessage::ExportEmbeddings { uris } = rt else {
            panic!("wrong variant");
        };
        assert_eq!(uris.len(), 1);
    }

    #[test]
    fn export_embeddings_is_batchable() {
        let msg = ClientMessage::ExportEmbeddings { uris: vec![] };
        assert!(msg.is_batchable());
    }

    #[test]
    fn cluster_result_round_trips() {
        let msg = ServerMessage::ClusterResult {
            groups: vec![
                vec!["file:///a.rs".into(), "file:///b.rs".into()],
                vec!["file:///c.rs".into()],
            ],
        };
        let rt = round_trip_server(&msg);
        let ServerMessage::ClusterResult { groups } = rt else {
            panic!("wrong variant");
        };
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].len(), 2);
    }

    #[test]
    fn export_embeddings_result_round_trips() {
        let mut embeddings = std::collections::HashMap::new();
        embeddings.insert("file:///a.rs".to_owned(), vec![0.1f32, 0.2, 0.3]);
        let msg = ServerMessage::ExportEmbeddingsResult { embeddings };
        let rt = round_trip_server(&msg);
        let ServerMessage::ExportEmbeddingsResult { embeddings } = rt else {
            panic!("wrong variant");
        };
        assert!(embeddings.contains_key("file:///a.rs"));
        assert_eq!(embeddings["file:///a.rs"].len(), 3);
    }
}
