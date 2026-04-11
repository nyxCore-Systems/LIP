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
            Self::Low    => f.write_str("low"),
            Self::Medium => f.write_str("medium"),
            Self::High   => f.write_str("high"),
        }
    }
}

/// A single file (or symbol within a file) that is transitively affected by
/// a change to a target symbol.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImpactItem {
    /// File URI of the affected file (`file:///…` or `lip://…`).
    pub file_uri:   String,
    /// URI of the specific symbol in that file that depends on the target.
    /// Empty when only file-level dependency graph data is available.
    pub symbol_uri: String,
    /// Distance from the target symbol in the call / dependency graph.
    /// `1` = direct caller, `2` = caller of caller, etc.
    pub distance:   u32,
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

/// A single fuzzy-search hit returned by `ClientMessage::SimilarSymbols`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimilarSymbol {
    pub uri:        String,
    pub name:       String,
    pub kind:       String,
    pub score:      f32,
    pub doc:        Option<String>,
    pub confidence: u8,
}

/// Result of `blast_radius(symbol_uri)`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BlastRadiusResult {
    pub symbol_uri:            String,
    /// Number of files that directly depend on the target symbol's file.
    /// Kept for backwards compatibility; prefer `direct_items.len()`.
    pub direct_dependents:     u32,
    /// Total number of transitively affected files.
    /// Kept for backwards compatibility; prefer `direct_items.len() + transitive_items.len()`.
    pub transitive_dependents: u32,
    /// All affected file URIs (direct + transitive), deduplicated.
    /// Kept for backwards compatibility; prefer `direct_items` + `transitive_items`.
    pub affected_files:        Vec<String>,
    /// Direct callers / dependents (distance = 1), richly typed.
    pub direct_items:          Vec<ImpactItem>,
    /// Transitive callers / dependents (distance ≥ 2), richly typed.
    pub transitive_items:      Vec<ImpactItem>,
    /// `true` when BFS was cut off by the depth or node limit.
    pub truncated:             bool,
    /// Composite risk level derived from caller count and spread.
    pub risk_level:            RiskLevel,
}

/// Result for a single sub-query inside a [`ClientMessage::BatchQuery`].
///
/// Exactly one of `ok` / `error` is set.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatchQueryResult {
    /// Successful response. `None` when `error` is set.
    pub ok:    Option<ServerMessage>,
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
        seq:      u64,
        accepted: bool,
        /// Set when `accepted` is false; describes why the delta was rejected.
        error:    Option<String>,
    },
    DeltaStream { deltas: Vec<crate::schema::OwnedDelta> },
    DefinitionResult {
        symbol:         Option<OwnedSymbolInfo>,
        /// URI of the file that contains the definition occurrence.
        /// `None` when no definition was found.
        location_uri:   Option<String>,
        /// Byte-offset range of the definition occurrence within `location_uri`.
        location_range: Option<OwnedRange>,
    },
    ReferencesResult { occurrences: Vec<crate::schema::OwnedOccurrence> },
    HoverResult { symbol: Option<OwnedSymbolInfo> },
    BlastRadiusResult(BlastRadiusResult),
    WorkspaceSymbolsResult { symbols: Vec<OwnedSymbolInfo> },
    DocumentSymbolsResult { symbols: Vec<OwnedSymbolInfo> },
    DeadSymbolsResult { symbols: Vec<OwnedSymbolInfo> },
    AnnotationAck,
    AnnotationValue { value: Option<String> },
    AnnotationEntries { entries: Vec<crate::schema::OwnedAnnotationEntry> },
    /// Response to a [`ClientMessage::BatchQuery`]. One result per input query, in order.
    BatchQueryResponse { results: Vec<BatchQueryResult> },
    /// Response to a [`ClientMessage::Batch`]. One `ServerMessage` per request, in order.
    BatchResult { results: Vec<ServerMessage> },
    /// Push notification: a symbol's confidence score was raised by Tier 2 verification.
    SymbolUpgraded {
        uri:            String,
        old_confidence: u8,
        new_confidence: u8,
    },
    /// Response to a [`ClientMessage::SimilarSymbols`] fuzzy search.
    SimilarSymbolsResult { symbols: Vec<SimilarSymbol> },
    Error { message: String },
}

/// Wire envelope for client → daemon messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Manifest(crate::daemon::manifest::ManifestRequest),
    Delta {
        /// Monotonically increasing client-side counter.
        /// The daemon echoes this in `DeltaAck.seq`.
        seq:      u64,
        action:   crate::schema::Action,
        document: crate::schema::OwnedDocument,
    },
    QueryDefinition {
        uri:  String,
        line: u32,
        col:  u32,
    },
    QueryReferences {
        symbol_uri: String,
        limit:      Option<usize>,
    },
    QueryHover {
        uri:  String,
        line: u32,
        col:  u32,
    },
    QueryBlastRadius {
        symbol_uri: String,
    },
    QueryWorkspaceSymbols {
        query: String,
        limit: Option<usize>,
    },
    QueryDocumentSymbols { uri: String },
    QueryDeadSymbols { limit: Option<usize> },
    AnnotationSet {
        symbol_uri: String,
        key:        String,
        value:      String,
        author_id:  String,
    },
    AnnotationGet {
        symbol_uri: String,
        key:        String,
    },
    AnnotationList { symbol_uri: String },
    /// Execute multiple queries in a single Unix socket round-trip.
    ///
    /// The daemon processes each sub-query under a single db lock acquisition and
    /// returns one [`BatchQueryResult`] per input query, preserving order.
    ///
    /// Restrictions: `Manifest`, `Delta`, and nested `BatchQuery` entries are
    /// rejected with an error entry rather than aborting the whole batch.
    BatchQuery { queries: Vec<ClientMessage> },
    /// Simple batch: execute multiple requests and return one `ServerMessage` per
    /// request, in order. Nested `Batch` entries are rejected immediately.
    Batch { requests: Vec<ClientMessage> },
    /// Trigram fuzzy-search across all tracked symbol names and documentation.
    SimilarSymbols { query: String, limit: usize },
}

impl ClientMessage {
    /// Returns `true` for any message that may appear inside a [`ClientMessage::Batch`].
    /// A `Batch` itself is excluded to prevent nesting.
    pub fn is_batchable(&self) -> bool {
        !matches!(self, ClientMessage::Batch { .. })
    }
}
