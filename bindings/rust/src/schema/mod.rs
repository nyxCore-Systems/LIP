//! Core owned types for the LIP wire protocol.
//!
//! Every type in this module is a plain Rust struct that mirrors a table in
//! [`schema/lip.fbs`](../../../../schema/lip.fbs). They are heap-allocated
//! ("Owned" prefix) so the rest of the codebase never needs to think about
//! FlatBuffers lifetimes. JSON serialisation is provided via `serde`; the
//! FlatBuffers zero-copy path is planned for v0.2.
//!
//! ## Key types
//!
//! | Type | Description |
//! |------|-------------|
//! | [`LipUri`] | Validated symbol URI (`lip://scope/pkg@ver/path#desc`) |
//! | [`OwnedSymbolInfo`] | A symbol with confidence score and telemetry |
//! | [`OwnedOccurrence`] | A use-site of a symbol at a source range |
//! | [`OwnedDocument`] | All symbols + occurrences in one file |
//! | [`OwnedDependencySlice`] | Pre-built index fragment from the registry |
//! | [`OwnedEventStream`] | Batch of [`OwnedDelta`]s emitted by the indexer |

pub mod types;

pub use types::{
    sha256_hex,
    Action, EdgeKind, IndexingState, LipUri,
    OwnedAnnotationEntry, OwnedDelta, OwnedDependencySlice, OwnedDocument,
    OwnedEventStream, OwnedGraphEdge, OwnedOccurrence, OwnedRange,
    OwnedRelationship, OwnedSymbolInfo, Role, SymbolKind,
};
