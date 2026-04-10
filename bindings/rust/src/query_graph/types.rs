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

/// Result of `blast_radius(symbol_uri)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BlastRadiusResult {
    pub symbol_uri:            String,
    pub direct_dependents:     u32,
    pub transitive_dependents: u32,
    pub affected_files:        Vec<String>,
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
}
