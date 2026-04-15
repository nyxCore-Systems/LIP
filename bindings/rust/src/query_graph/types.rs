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
    /// Confidence schedule for blast-radius weighting.
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

/// A line-range chunk boundary returned by [`ServerMessage::BoundariesResult`].
///
/// `[start_line, end_line]` is the chunk *before* the semantic shift.
/// `shift_magnitude` is the cosine distance to the next chunk — higher means a sharper boundary.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoundaryRange {
    pub start_line: u32,
    pub end_line: u32,
    /// Cosine distance in `[0.0, 2.0]` between this chunk and the following one.
    pub shift_magnitude: f32,
}

/// Per-file novelty score in a [`ServerMessage::NoveltyScoreResult`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NoveltyItem {
    pub uri: String,
    /// `1 − similarity_to_nearest_existing_file`. Range `[0.0, 1.0]`; higher = more novel.
    pub score: f32,
    /// The most semantically similar file *outside* the input set, or `None` when the index
    /// has no other files with embeddings.
    pub nearest_existing: Option<String>,
}

/// A domain term returned by [`ServerMessage::TerminologyResult`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TermItem {
    /// Symbol display name that is semantically central to the input file set.
    pub term: String,
    /// Cosine similarity to the centroid of the input files' embeddings.
    pub score: f32,
    /// URI of the file that defines this symbol.
    pub source_uri: String,
}

/// Per-directory breakdown inside a [`ServerMessage::CoverageResult`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DirectoryCoverage {
    /// The directory URI prefix (e.g. `file:///project/src`).
    pub directory: String,
    /// Number of indexed files under this directory.
    pub total_files: usize,
    /// Number of those files that have a cached embedding.
    pub embedded_files: usize,
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
        /// `true` when the index contains embeddings produced by more than one model.
        /// Mixed-model indexes are unreliable for cosine search — re-embed to resolve.
        mixed_models: bool,
        /// Distinct model names present across all stored file embeddings, sorted.
        models_in_index: Vec<String>,
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
        /// The model that produced this file's embedding, if known.
        embedding_model: Option<String>,
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

    // ── v1.6 features ────────────────────────────────────────────────────
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

    // ── v1.7 features ────────────────────────────────────────────────────
    /// Response to [`ClientMessage::QueryNearestByContrast`] and
    /// [`ClientMessage::FindSemanticCounterpart`].
    ///
    /// Reuses [`NearestResult`] — same shape, different query semantics.

    /// Response to [`ClientMessage::QueryOutliers`].
    ///
    /// `outliers[i].score` is the leave-one-out mean cosine similarity to the rest of the
    /// input set — **lower score = more outlier-like**.
    OutliersResult {
        outliers: Vec<NearestItem>,
    },

    /// Response to [`ClientMessage::QuerySemanticDrift`].
    ///
    /// `distance` is the cosine distance `1 − similarity` in `[0.0, 2.0]`.
    /// `None` when either URI has no cached embedding.
    SemanticDriftResult {
        distance: Option<f32>,
    },

    /// Response to [`ClientMessage::SimilarityMatrix`].
    ///
    /// `uris[i]` corresponds to row/column `i` of `matrix`.
    /// URIs from the input without a cached embedding are silently excluded.
    SimilarityMatrixResult {
        /// URIs present in the matrix, in row order.
        uris: Vec<String>,
        /// Row-major N×N cosine-similarity matrix. `matrix[i][j]` = sim(`uris[i]`, `uris[j]`).
        matrix: Vec<Vec<f32>>,
    },

    /// Response to [`ClientMessage::QueryCoverage`].
    CoverageResult {
        /// The root path that was queried.
        root: String,
        /// Total indexed files under `root`.
        total_files: usize,
        /// Files under `root` that have a cached embedding.
        embedded_files: usize,
        /// `embedded_files / total_files`. `None` when `total_files == 0`.
        coverage_fraction: Option<f32>,
        /// Per-directory breakdown, sorted by directory path.
        by_directory: Vec<DirectoryCoverage>,
    },

    // ── v1.8 features ────────────────────────────────────────────────────
    /// Response to [`ClientMessage::FindBoundaries`].
    BoundariesResult {
        /// The file that was scanned.
        uri: String,
        /// Chunk boundaries ordered by line number. Only chunks above `threshold` are returned.
        boundaries: Vec<BoundaryRange>,
    },

    /// Response to [`ClientMessage::SemanticDiff`].
    SemanticDiffResult {
        /// Cosine distance `1 − similarity` between the two content embeddings. `[0.0, 2.0]`.
        distance: f32,
        /// Nearest files to the *direction* the content moved (i.e. nearest to `new − old`).
        moving_toward: Vec<NearestItem>,
    },

    /// Response to [`ClientMessage::QueryNoveltyScore`].
    NoveltyScoreResult {
        /// Mean novelty across all scored input files. `0.0` when no file had an embedding.
        score: f32,
        /// Per-file breakdown, sorted by descending novelty score.
        per_file: Vec<NoveltyItem>,
    },

    /// Response to [`ClientMessage::ExtractTerminology`].
    TerminologyResult {
        /// Domain terms ranked by semantic centrality to the input file set.
        terms: Vec<TermItem>,
    },

    /// Response to [`ClientMessage::PruneDeleted`].
    PruneDeletedResult {
        /// Number of tracked file URIs that were checked against the filesystem.
        checked: usize,
        /// URIs that no longer exist on disk and were removed from the index.
        removed: Vec<String>,
    },

    // ── v1.9 features ────────────────────────────────────────────────────
    /// Response to [`ClientMessage::GetCentroid`].
    CentroidResult {
        /// Mean embedding vector of the included files.  Empty when no URI had
        /// a cached embedding.
        vector: Vec<f32>,
        /// Number of input URIs that contributed to the centroid.
        included: usize,
    },

    /// Response to [`ClientMessage::QueryStaleEmbeddings`].
    StaleEmbeddingsResult {
        /// File URIs whose stored embedding is older than the file's current
        /// mtime, or whose index timestamp is unknown.
        uris: Vec<String>,
    },

    // ── v2.0 features ────────────────────────────────────────────────────
    /// Response to [`ClientMessage::ExplainMatch`].
    ExplainMatchResult {
        /// Top-scoring chunks of `result_uri`, ordered by descending contribution score.
        chunks: Vec<ExplanationChunk>,
        /// The embedding model used to score the chunks.
        query_model: String,
    },

    // ── v2.1 features ────────────────────────────────────────────────────
    /// One frame of a [`ClientMessage::StreamContext`] response: a single
    /// ranked symbol with its estimated prompt token cost.
    ///
    /// Wire tag is `"symbol_info"`. Multiple frames precede the
    /// [`ServerMessage::EndStream`] terminator.
    SymbolInfo {
        symbol_info: crate::schema::OwnedSymbolInfo,
        /// Heuristic score in `[0.0, 1.0]`; higher = more relevant to the cursor.
        relevance_score: f32,
        /// Estimated prompt-token cost of this symbol's serialised context.
        token_cost: u32,
    },

    /// Terminator frame for a [`ClientMessage::StreamContext`] response.
    ///
    /// Wire tag is `"end_stream"`. Exactly one terminator follows N
    /// [`ServerMessage::SymbolInfo`] frames.
    EndStream {
        reason: EndStreamReason,
        /// Number of `SymbolInfo` frames emitted before this terminator.
        emitted: u32,
        /// Total candidate symbols the daemon considered.
        total_candidates: u32,
        /// Set only when `reason == EndStreamReason::Error`.
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

/// Why a [`ServerMessage::EndStream`] terminated a context stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndStreamReason {
    /// Daemon emitted enough symbols to reach `max_tokens`.
    BudgetReached,
    /// No more relevant candidates exist.
    Exhausted,
    /// An error terminated the stream; see [`ServerMessage::EndStream::error`].
    Error,
}

/// A contiguous region of a file that contributes to a semantic match.
///
/// Returned as part of [`ServerMessage::ExplainMatchResult`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ExplanationChunk {
    /// 0-based first line of this chunk (inclusive).
    pub start_line: u32,
    /// 0-based last line of this chunk (inclusive).
    pub end_line: u32,
    /// The source text of this chunk.
    pub chunk_text: String,
    /// Cosine similarity of this chunk against the query embedding.
    pub score: f32,
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
        /// Optional glob pattern to restrict candidates (e.g. `"internal/auth/**"` or
        /// `"*_test.go"`). Patterns with a `/` are matched against the full path;
        /// patterns without are matched against the filename only.
        filter: Option<String>,
        /// Minimum cosine similarity to include in results. Items scoring below this
        /// threshold are discarded rather than returned as low-confidence noise.
        min_score: Option<f32>,
    },
    /// Find the `top_k` files whose stored embedding is most similar to the given text.
    /// The daemon embeds `text` on the fly and runs cosine search.
    /// Returns `NearestResult`.
    QueryNearestByText {
        text: String,
        top_k: usize,
        model: Option<String>,
        /// See [`ClientMessage::QueryNearest::filter`].
        filter: Option<String>,
        /// See [`ClientMessage::QueryNearest::min_score`].
        min_score: Option<f32>,
    },
    /// Embed multiple query strings in one round-trip and return the top-k nearest
    /// files for each. Returns `BatchNearestResult`.
    BatchQueryNearestByText {
        queries: Vec<String>,
        top_k: usize,
        model: Option<String>,
        /// See [`ClientMessage::QueryNearest::filter`]. Applied to all queries.
        filter: Option<String>,
        /// See [`ClientMessage::QueryNearest::min_score`]. Applied to all queries.
        min_score: Option<f32>,
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

    // ── v1.6 features ────────────────────────────────────────────────────
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

    // ── v1.7 features ────────────────────────────────────────────────────
    /// Contrastive nearest-neighbour search using vector arithmetic.
    ///
    /// Computes `normalize(embed(like_uri) − embed(unlike_uri))` then finds the
    /// `top_k` files most similar to that direction.  Both URIs must have cached
    /// embeddings — call `EmbeddingBatch` first if needed.
    /// Returns `NearestResult`.
    QueryNearestByContrast {
        /// URI of the file whose embedding we want to move *towards*.
        like_uri: String,
        /// URI of the file whose embedding we want to move *away from*.
        unlike_uri: String,
        top_k: usize,
        /// See [`ClientMessage::QueryNearest::filter`].
        filter: Option<String>,
        /// See [`ClientMessage::QueryNearest::min_score`].
        min_score: Option<f32>,
    },

    /// Return the `top_k` files from `uris` that are most semantically dissimilar
    /// from the rest of the group.
    ///
    /// Uses leave-one-out mean cosine similarity: for each URI compute the mean
    /// similarity to all other URIs in the set; the lowest-scoring URIs are the
    /// outliers. URIs without a cached embedding are silently excluded.
    /// Returns `OutliersResult`.
    QueryOutliers {
        uris: Vec<String>,
        top_k: usize,
    },

    /// Compute the semantic drift between two URIs as a cosine distance scalar.
    ///
    /// `distance = 1 − cosine_similarity`.  Range `[0.0, 2.0]`; `0.0` = identical.
    /// Returns `None` when either URI has no cached embedding.
    /// Returns `SemanticDriftResult`.
    QuerySemanticDrift {
        uri_a: String,
        uri_b: String,
    },

    /// Compute all pairwise cosine similarities for a list of URIs in one call.
    ///
    /// Only URIs that already have a cached embedding are included in the result;
    /// the rest are silently excluded.  Returns `SimilarityMatrixResult`.
    SimilarityMatrix {
        uris: Vec<String>,
    },

    /// Given a source URI and a pool of candidate URIs, return the `top_k` candidates
    /// most semantically similar to the source.
    ///
    /// The source must have a cached embedding.  Candidates without embeddings are
    /// silently excluded.  Returns `NearestResult`.
    FindSemanticCounterpart {
        /// The file (or symbol) to match against.
        uri: String,
        /// Candidate URIs to rank.
        candidates: Vec<String>,
        top_k: usize,
        /// See [`ClientMessage::QueryNearest::filter`].
        filter: Option<String>,
        /// See [`ClientMessage::QueryNearest::min_score`].
        min_score: Option<f32>,
    },

    /// Report how much of the index under a filesystem root has embedding coverage.
    ///
    /// `root` is matched as a path prefix against `file://` URIs tracked by the daemon
    /// (e.g. `"/project/src"` matches `file:///project/src/foo.rs`).
    /// Returns `CoverageResult`.
    QueryCoverage {
        /// Filesystem path prefix to scope the report (e.g. `"/project/src"`).
        root: String,
    },

    // ── v1.8 features ────────────────────────────────────────────────────
    /// Detect semantic boundaries within a file by chunking and embedding.
    ///
    /// Splits the file's source text into windows of `chunk_lines` lines, embeds each window,
    /// and returns the positions where cosine distance between adjacent windows exceeds
    /// `threshold`. Useful for identifying natural split points during extract refactors.
    /// Requires `LIP_EMBEDDING_URL`. Returns `BoundariesResult`.
    FindBoundaries {
        /// File URI to scan.
        uri: String,
        /// Number of lines per embedding window. Default 30.
        chunk_lines: usize,
        /// Minimum cosine distance to report as a boundary. Default 0.3.
        threshold: f32,
        model: Option<String>,
    },

    /// Measure how much the semantic content of a file has changed between two versions.
    ///
    /// Embeds `content_a` (old) and `content_b` (new), returns:
    /// - `distance`: cosine distance `1 − similarity` — the drift magnitude.
    /// - `moving_toward`: `top_k` nearest files to the *direction* of change
    ///   (`normalize(new − old)`), naming what concepts the content moved towards.
    ///
    /// Requires `LIP_EMBEDDING_URL`. Returns `SemanticDiffResult`.
    SemanticDiff {
        content_a: String,
        content_b: String,
        top_k: usize,
        model: Option<String>,
    },

    /// Semantic nearest-neighbour search against a caller-provided embedding store.
    ///
    /// Useful for cross-repo federation: export embeddings from each repo root via
    /// `ExportEmbeddings`, merge the maps, then query across all roots in one call.
    /// The query `uri` must have a cached embedding in the daemon's own index.
    /// Returns `NearestResult`.
    QueryNearestInStore {
        /// The file whose embedding is used as the query vector.
        uri: String,
        /// External embedding store: map of URI → embedding vector.
        store: std::collections::HashMap<String, Vec<f32>>,
        top_k: usize,
        /// See [`ClientMessage::QueryNearest::filter`].
        filter: Option<String>,
        /// See [`ClientMessage::QueryNearest::min_score`].
        min_score: Option<f32>,
    },

    /// Compute how semantically novel a set of files is relative to the existing codebase.
    ///
    /// For each URI in `uris`, finds its nearest neighbour *outside* the set and returns
    /// `1 − similarity` as that file's novelty score. The overall `score` is the mean.
    /// URIs without a cached embedding are skipped. Returns `NoveltyScoreResult`.
    QueryNoveltyScore {
        uris: Vec<String>,
    },

    /// Extract the domain vocabulary most semantically central to a set of files.
    ///
    /// Computes the centroid of the input files' embeddings, then scores each symbol
    /// defined in those files by its embedding's similarity to the centroid. Returns
    /// the `top_k` most central symbol display names.
    ///
    /// Requires symbol embeddings — call `EmbeddingBatch` with `lip://` URIs first.
    /// Returns `TerminologyResult`.
    ExtractTerminology {
        uris: Vec<String>,
        top_k: usize,
    },

    /// Remove index entries for files that no longer exist on disk.
    ///
    /// Iterates all tracked file URIs, checks each against the filesystem, and
    /// removes stale entries (including their embeddings). Returns `PruneDeletedResult`.
    PruneDeleted,

    // ── v1.9 features ────────────────────────────────────────────────────
    /// Compute and return the embedding centroid of a set of files without
    /// shipping all raw vectors to the caller.
    ///
    /// The centroid is the component-wise mean of each file's stored embedding.
    /// URIs without a cached embedding are silently excluded.  Returns `CentroidResult`.
    GetCentroid {
        /// File (or symbol) URIs to average.
        uris: Vec<String>,
    },

    /// Report which files under `root` have a stale embedding.
    ///
    /// A file's embedding is considered stale when its filesystem mtime is
    /// newer than the daemon's `file_indexed_at` timestamp, meaning the content
    /// changed while the daemon was offline.  Files with no `indexed_at` record
    /// are also reported as stale (conservative).
    ///
    /// Returns `StaleEmbeddingsResult`.
    QueryStaleEmbeddings {
        /// Filesystem path prefix to scope the scan (e.g. `"/project/src"`).
        root: String,
    },

    // ── v2.0 features ────────────────────────────────────────────────────
    /// Explain *why* `result_uri` was ranked as a strong semantic match for `query`.
    ///
    /// The daemon chunks `result_uri`'s source text into `chunk_lines`-line windows,
    /// embeds each chunk, then cosine-scores each against the query embedding
    /// (cached for URI queries; computed on the fly for text queries).
    /// Returns the top `top_k` chunks in descending score order.
    ///
    /// Returns `ExplainMatchResult`.
    ExplainMatch {
        /// Either a file URI (`file://…`) to use its cached embedding, or a
        /// free-text query to embed on the fly.
        query: String,
        /// The file URI whose source will be chunked and scored.
        result_uri: String,
        /// Number of top-scoring chunks to return. Defaults to 5 if 0 is passed.
        top_k: usize,
        /// Lines per chunk window. Defaults to 20 if 0 is passed.
        chunk_lines: usize,
        /// Override the embedding model for this request.
        model: Option<String>,
    },

    // ── v2.1 features ────────────────────────────────────────────────────
    /// Stream symbols ordered by relevance to `cursor_position` in `file_uri`,
    /// stopping when the caller closes the connection or when the daemon has
    /// emitted enough symbols to reach `max_tokens` estimated prompt cost.
    ///
    /// Response is N [`ServerMessage::SymbolInfo`] frames followed by exactly
    /// one [`ServerMessage::EndStream`] terminator.
    StreamContext {
        file_uri: String,
        cursor_position: OwnedRange,
        max_tokens: u32,
        /// Optional: restrict to a specific embedding model.
        #[serde(default)]
        model: Option<String>,
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
                | ClientMessage::FindBoundaries { .. }
                | ClientMessage::SemanticDiff { .. }
                | ClientMessage::PruneDeleted
                | ClientMessage::QueryStaleEmbeddings { .. }
                | ClientMessage::ExplainMatch { .. }
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
            filter: None,
            min_score: None,
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
            protocol_version: 2,
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
        assert_eq!(protocol_version, 2);
    }

    #[test]
    fn stream_context_request_round_trips() {
        let msg = ClientMessage::StreamContext {
            file_uri: "file:///src/main.rs".into(),
            cursor_position: OwnedRange {
                start_line: 10,
                start_char: 4,
                end_line: 10,
                end_char: 4,
            },
            max_tokens: 4096,
            model: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "stream_context");
        assert_eq!(json["max_tokens"], 4096);
        let rt = round_trip_client(&msg);
        let ClientMessage::StreamContext {
            file_uri,
            max_tokens,
            ..
        } = rt
        else {
            panic!("wrong variant");
        };
        assert_eq!(file_uri, "file:///src/main.rs");
        assert_eq!(max_tokens, 4096);
    }

    #[test]
    fn end_stream_frame_round_trips() {
        let msg = ServerMessage::EndStream {
            reason: EndStreamReason::BudgetReached,
            emitted: 3,
            total_candidates: 12,
            error: None,
        };
        let json = serde_json::to_value(&msg).unwrap();
        assert_eq!(json["type"], "end_stream");
        assert_eq!(json["reason"], "budget_reached");
        // Optional `error` field omitted when None.
        assert!(json.get("error").is_none());

        let rt = round_trip_server(&msg);
        let ServerMessage::EndStream {
            reason,
            emitted,
            total_candidates,
            error,
        } = rt
        else {
            panic!("wrong variant");
        };
        assert_eq!(reason, EndStreamReason::BudgetReached);
        assert_eq!(emitted, 3);
        assert_eq!(total_candidates, 12);
        assert!(error.is_none());
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
            filter: None,
            min_score: None,
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

    // ── v1.7 round-trip tests ─────────────────────────────────────────────

    #[test]
    fn query_nearest_by_contrast_round_trips() {
        let msg = ClientMessage::QueryNearestByContrast {
            like_uri: "file:///src/new_auth.rs".into(),
            unlike_uri: "file:///src/legacy_auth.rs".into(),
            top_k: 5,
            filter: None,
            min_score: None,
        };
        let rt = round_trip_client(&msg);
        let ClientMessage::QueryNearestByContrast {
            like_uri,
            unlike_uri,
            top_k,
            ..
        } = rt
        else {
            panic!("wrong variant");
        };
        assert_eq!(like_uri, "file:///src/new_auth.rs");
        assert_eq!(unlike_uri, "file:///src/legacy_auth.rs");
        assert_eq!(top_k, 5);
    }

    #[test]
    fn query_outliers_round_trips() {
        let msg = ClientMessage::QueryOutliers {
            uris: vec!["file:///src/a.rs".into(), "file:///src/b.rs".into()],
            top_k: 3,
        };
        let rt = round_trip_client(&msg);
        let ClientMessage::QueryOutliers { uris, top_k } = rt else {
            panic!("wrong variant");
        };
        assert_eq!(uris.len(), 2);
        assert_eq!(top_k, 3);
    }

    #[test]
    fn outliers_result_round_trips() {
        let msg = ServerMessage::OutliersResult {
            outliers: vec![NearestItem {
                uri: "file:///src/billing.go".into(),
                score: 0.12,
            }],
        };
        let rt = round_trip_server(&msg);
        let ServerMessage::OutliersResult { outliers } = rt else {
            panic!("wrong variant");
        };
        assert_eq!(outliers.len(), 1);
        assert!((outliers[0].score - 0.12).abs() < 1e-5);
    }

    #[test]
    fn query_semantic_drift_round_trips() {
        let msg = ClientMessage::QuerySemanticDrift {
            uri_a: "file:///a.rs".into(),
            uri_b: "file:///b.rs".into(),
        };
        let rt = round_trip_client(&msg);
        let ClientMessage::QuerySemanticDrift { uri_a, uri_b } = rt else {
            panic!("wrong variant");
        };
        assert_eq!(uri_a, "file:///a.rs");
        assert_eq!(uri_b, "file:///b.rs");
    }

    #[test]
    fn semantic_drift_result_round_trips() {
        let msg = ServerMessage::SemanticDriftResult {
            distance: Some(0.42),
        };
        let rt = round_trip_server(&msg);
        let ServerMessage::SemanticDriftResult { distance } = rt else {
            panic!("wrong variant");
        };
        assert!((distance.unwrap() - 0.42).abs() < 1e-5);
    }

    #[test]
    fn similarity_matrix_round_trips() {
        let msg = ClientMessage::SimilarityMatrix {
            uris: vec!["file:///a.rs".into(), "file:///b.rs".into()],
        };
        let rt = round_trip_client(&msg);
        let ClientMessage::SimilarityMatrix { uris } = rt else {
            panic!("wrong variant");
        };
        assert_eq!(uris.len(), 2);
    }

    #[test]
    fn similarity_matrix_result_round_trips() {
        let msg = ServerMessage::SimilarityMatrixResult {
            uris: vec!["file:///a.rs".into(), "file:///b.rs".into()],
            matrix: vec![vec![1.0, 0.7], vec![0.7, 1.0]],
        };
        let rt = round_trip_server(&msg);
        let ServerMessage::SimilarityMatrixResult { uris, matrix } = rt else {
            panic!("wrong variant");
        };
        assert_eq!(uris.len(), 2);
        assert!((matrix[0][1] - 0.7).abs() < 1e-5);
        assert!((matrix[1][0] - 0.7).abs() < 1e-5);
    }

    #[test]
    fn find_semantic_counterpart_round_trips() {
        let msg = ClientMessage::FindSemanticCounterpart {
            uri: "file:///src/auth.rs".into(),
            candidates: vec![
                "file:///tests/auth_test.rs".into(),
                "file:///tests/other_test.rs".into(),
            ],
            top_k: 1,
            filter: None,
            min_score: None,
        };
        let rt = round_trip_client(&msg);
        let ClientMessage::FindSemanticCounterpart {
            uri,
            candidates,
            top_k,
            ..
        } = rt
        else {
            panic!("wrong variant");
        };
        assert_eq!(uri, "file:///src/auth.rs");
        assert_eq!(candidates.len(), 2);
        assert_eq!(top_k, 1);
    }

    #[test]
    fn query_coverage_round_trips() {
        let msg = ClientMessage::QueryCoverage {
            root: "/project/src".into(),
        };
        let rt = round_trip_client(&msg);
        let ClientMessage::QueryCoverage { root } = rt else {
            panic!("wrong variant");
        };
        assert_eq!(root, "/project/src");
    }

    #[test]
    fn coverage_result_round_trips() {
        let msg = ServerMessage::CoverageResult {
            root: "/project/src".into(),
            total_files: 10,
            embedded_files: 7,
            coverage_fraction: Some(0.7),
            by_directory: vec![DirectoryCoverage {
                directory: "file:///project/src".into(),
                total_files: 10,
                embedded_files: 7,
            }],
        };
        let rt = round_trip_server(&msg);
        let ServerMessage::CoverageResult {
            total_files,
            embedded_files,
            coverage_fraction,
            by_directory,
            ..
        } = rt
        else {
            panic!("wrong variant");
        };
        assert_eq!(total_files, 10);
        assert_eq!(embedded_files, 7);
        assert!((coverage_fraction.unwrap() - 0.7).abs() < 1e-5);
        assert_eq!(by_directory.len(), 1);
    }

    // ── v1.8 round-trip tests ─────────────────────────────────────────────

    #[test]
    fn find_boundaries_round_trips() {
        let msg = ClientMessage::FindBoundaries {
            uri: "file:///src/large.rs".into(),
            chunk_lines: 30,
            threshold: 0.3,
            model: None,
        };
        let rt = round_trip_client(&msg);
        let ClientMessage::FindBoundaries {
            uri,
            chunk_lines,
            threshold,
            ..
        } = rt
        else {
            panic!("wrong variant");
        };
        assert_eq!(uri, "file:///src/large.rs");
        assert_eq!(chunk_lines, 30);
        assert!((threshold - 0.3).abs() < 1e-5);
    }

    #[test]
    fn boundaries_result_round_trips() {
        let msg = ServerMessage::BoundariesResult {
            uri: "file:///src/large.rs".into(),
            boundaries: vec![BoundaryRange {
                start_line: 0,
                end_line: 29,
                shift_magnitude: 0.55,
            }],
        };
        let rt = round_trip_server(&msg);
        let ServerMessage::BoundariesResult { uri, boundaries } = rt else {
            panic!("wrong variant");
        };
        assert_eq!(uri, "file:///src/large.rs");
        assert_eq!(boundaries.len(), 1);
        assert!((boundaries[0].shift_magnitude - 0.55).abs() < 1e-5);
    }

    #[test]
    fn semantic_diff_round_trips() {
        let msg = ClientMessage::SemanticDiff {
            content_a: "old content".into(),
            content_b: "new content".into(),
            top_k: 5,
            model: None,
        };
        let rt = round_trip_client(&msg);
        let ClientMessage::SemanticDiff {
            content_a,
            content_b,
            top_k,
            ..
        } = rt
        else {
            panic!("wrong variant");
        };
        assert_eq!(content_a, "old content");
        assert_eq!(content_b, "new content");
        assert_eq!(top_k, 5);
    }

    #[test]
    fn semantic_diff_result_round_trips() {
        let msg = ServerMessage::SemanticDiffResult {
            distance: 0.22,
            moving_toward: vec![NearestItem {
                uri: "file:///src/auth.rs".into(),
                score: 0.91,
            }],
        };
        let rt = round_trip_server(&msg);
        let ServerMessage::SemanticDiffResult {
            distance,
            moving_toward,
        } = rt
        else {
            panic!("wrong variant");
        };
        assert!((distance - 0.22).abs() < 1e-5);
        assert_eq!(moving_toward.len(), 1);
    }

    #[test]
    fn query_nearest_in_store_round_trips() {
        let mut store = std::collections::HashMap::new();
        store.insert("file:///other/a.rs".to_owned(), vec![1.0f32, 0.0]);
        let msg = ClientMessage::QueryNearestInStore {
            uri: "file:///src/auth.rs".into(),
            store,
            top_k: 3,
            filter: None,
            min_score: None,
        };
        let rt = round_trip_client(&msg);
        let ClientMessage::QueryNearestInStore {
            uri, store, top_k, ..
        } = rt
        else {
            panic!("wrong variant");
        };
        assert_eq!(uri, "file:///src/auth.rs");
        assert_eq!(top_k, 3);
        assert!(store.contains_key("file:///other/a.rs"));
    }

    #[test]
    fn query_novelty_score_round_trips() {
        let msg = ClientMessage::QueryNoveltyScore {
            uris: vec!["file:///src/new.rs".into()],
        };
        let rt = round_trip_client(&msg);
        let ClientMessage::QueryNoveltyScore { uris } = rt else {
            panic!("wrong variant");
        };
        assert_eq!(uris.len(), 1);
    }

    #[test]
    fn novelty_score_result_round_trips() {
        let msg = ServerMessage::NoveltyScoreResult {
            score: 0.65,
            per_file: vec![NoveltyItem {
                uri: "file:///src/new.rs".into(),
                score: 0.65,
                nearest_existing: Some("file:///src/auth.rs".into()),
            }],
        };
        let rt = round_trip_server(&msg);
        let ServerMessage::NoveltyScoreResult { score, per_file } = rt else {
            panic!("wrong variant");
        };
        assert!((score - 0.65).abs() < 1e-5);
        assert_eq!(per_file.len(), 1);
        assert_eq!(
            per_file[0].nearest_existing.as_deref(),
            Some("file:///src/auth.rs")
        );
    }

    #[test]
    fn extract_terminology_round_trips() {
        let msg = ClientMessage::ExtractTerminology {
            uris: vec!["file:///src/auth.rs".into()],
            top_k: 10,
        };
        let rt = round_trip_client(&msg);
        let ClientMessage::ExtractTerminology { uris, top_k } = rt else {
            panic!("wrong variant");
        };
        assert_eq!(uris.len(), 1);
        assert_eq!(top_k, 10);
    }

    #[test]
    fn terminology_result_round_trips() {
        let msg = ServerMessage::TerminologyResult {
            terms: vec![TermItem {
                term: "authenticate".into(),
                score: 0.88,
                source_uri: "file:///src/auth.rs".into(),
            }],
        };
        let rt = round_trip_server(&msg);
        let ServerMessage::TerminologyResult { terms } = rt else {
            panic!("wrong variant");
        };
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].term, "authenticate");
    }

    #[test]
    fn prune_deleted_round_trips() {
        let msg = ClientMessage::PruneDeleted;
        let rt = round_trip_client(&msg);
        assert!(matches!(rt, ClientMessage::PruneDeleted));
    }

    #[test]
    fn prune_deleted_result_round_trips() {
        let msg = ServerMessage::PruneDeletedResult {
            checked: 42,
            removed: vec!["file:///src/gone.rs".into()],
        };
        let rt = round_trip_server(&msg);
        let ServerMessage::PruneDeletedResult { checked, removed } = rt else {
            panic!("wrong variant");
        };
        assert_eq!(checked, 42);
        assert_eq!(removed.len(), 1);
    }

    #[test]
    fn find_boundaries_not_batchable() {
        let msg = ClientMessage::FindBoundaries {
            uri: "file:///src/f.rs".into(),
            chunk_lines: 30,
            threshold: 0.3,
            model: None,
        };
        assert!(!msg.is_batchable());
    }

    #[test]
    fn semantic_diff_not_batchable() {
        let msg = ClientMessage::SemanticDiff {
            content_a: String::new(),
            content_b: String::new(),
            top_k: 5,
            model: None,
        };
        assert!(!msg.is_batchable());
    }

    #[test]
    fn prune_deleted_not_batchable() {
        assert!(!ClientMessage::PruneDeleted.is_batchable());
    }

    #[test]
    fn query_nearest_in_store_is_batchable() {
        let msg = ClientMessage::QueryNearestInStore {
            uri: "file:///a.rs".into(),
            store: std::collections::HashMap::new(),
            top_k: 5,
            filter: None,
            min_score: None,
        };
        assert!(msg.is_batchable());
    }

    #[test]
    fn query_novelty_score_is_batchable() {
        let msg = ClientMessage::QueryNoveltyScore { uris: vec![] };
        assert!(msg.is_batchable());
    }

    #[test]
    fn extract_terminology_is_batchable() {
        let msg = ClientMessage::ExtractTerminology {
            uris: vec![],
            top_k: 10,
        };
        assert!(msg.is_batchable());
    }

    // ── v1.9 round-trip tests ─────────────────────────────────────────────

    #[test]
    fn get_centroid_round_trips() {
        let msg = ClientMessage::GetCentroid {
            uris: vec!["file:///src/auth.rs".into(), "file:///src/db.rs".into()],
        };
        let rt = round_trip_client(&msg);
        let ClientMessage::GetCentroid { uris } = rt else {
            panic!("wrong variant");
        };
        assert_eq!(uris.len(), 2);
    }

    #[test]
    fn centroid_result_round_trips() {
        let msg = ServerMessage::CentroidResult {
            vector: vec![0.1, 0.2, 0.3],
            included: 2,
        };
        let rt = round_trip_server(&msg);
        let ServerMessage::CentroidResult { vector, included } = rt else {
            panic!("wrong variant");
        };
        assert_eq!(vector.len(), 3);
        assert_eq!(included, 2);
    }

    #[test]
    fn get_centroid_is_batchable() {
        let msg = ClientMessage::GetCentroid { uris: vec![] };
        assert!(msg.is_batchable());
    }

    #[test]
    fn query_stale_embeddings_round_trips() {
        let msg = ClientMessage::QueryStaleEmbeddings {
            root: "/project/src".into(),
        };
        let rt = round_trip_client(&msg);
        let ClientMessage::QueryStaleEmbeddings { root } = rt else {
            panic!("wrong variant");
        };
        assert_eq!(root, "/project/src");
    }

    #[test]
    fn stale_embeddings_result_round_trips() {
        let msg = ServerMessage::StaleEmbeddingsResult {
            uris: vec!["file:///src/auth.rs".into()],
        };
        let rt = round_trip_server(&msg);
        let ServerMessage::StaleEmbeddingsResult { uris } = rt else {
            panic!("wrong variant");
        };
        assert_eq!(uris.len(), 1);
    }

    #[test]
    fn query_stale_embeddings_not_batchable() {
        let msg = ClientMessage::QueryStaleEmbeddings {
            root: "/project".into(),
        };
        assert!(!msg.is_batchable());
    }

    #[test]
    fn filter_and_min_score_round_trip_on_nearest() {
        let msg = ClientMessage::QueryNearest {
            uri: "file:///src/auth.rs".into(),
            top_k: 5,
            filter: Some("internal/**".into()),
            min_score: Some(0.5),
        };
        let rt = round_trip_client(&msg);
        let ClientMessage::QueryNearest {
            filter, min_score, ..
        } = rt
        else {
            panic!("wrong variant");
        };
        assert_eq!(filter.as_deref(), Some("internal/**"));
        assert!((min_score.unwrap() - 0.5).abs() < 1e-5);
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
