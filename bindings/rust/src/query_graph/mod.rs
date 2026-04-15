//! Salsa-inspired incremental query graph (spec §3.1).
//!
//! The query graph sits between file inputs and the daemon: it decides *what*
//! to recompute and *when* to short-circuit.
//!
//! ## Design
//!
//! ```text
//! upsert_file(uri, text, lang)
//!       │
//!       ▼
//! file_symbols(uri)   ──►  file_api_surface(uri)  ◄── early-cutoff node
//! file_occurrences(uri)           │
//!                                 ▼
//!                          reverse_deps(uri)
//!                                 │
//!                                 ▼
//!                         blast_radius_for(symbol)
//! ```
//!
//! Each derived query stores `(value, revision)`. A query is *fresh* when its
//! stored revision ≥ the current revision of its input file. Stale entries are
//! recomputed on demand.
//!
//! [`LipDatabase::file_api_surface`] is the **early-cutoff node**: if the
//! recomputed API-surface hash equals the cached hash, the old `Arc` is
//! returned unchanged. Downstream callers that hold a clone of that `Arc` can
//! detect no-change by pointer equality, avoiding their own recomputation.
//!
//! ## Why not the `salsa` crate?
//!
//! The salsa proc-macro API changed incompatibly between 0.16 and current
//! main. Rather than pin to a stale release, v0.1 implements the same
//! invariant manually. A migration to the upstream crate is on the v0.2
//! roadmap.

pub mod db;
pub mod types;

pub use db::LipDatabase;
pub use types::{
    ApiSurface, BatchQueryResult, BlastRadiusResult, ClientMessage, ErrorCode, ImpactItem,
    RiskLevel, ServerMessage, SimilarSymbol, Tier3Source,
};
