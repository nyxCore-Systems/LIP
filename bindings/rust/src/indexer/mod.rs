//! Tier 1 tree-sitter indexer (spec §3.3).
//!
//! Tier 1 provides fast, syntax-only code intelligence within ~10 ms per file.
//! Confidence scores fall in the range 1–50 (symbols: 30, occurrences: 20).
//!
//! ## Supported languages (v0.1)
//!
//! | Language   | Grammar crate              | Status |
//! |------------|----------------------------|--------|
//! | Rust       | `tree-sitter-rust`         | Full   |
//! | TypeScript | `tree-sitter-typescript`   | Full   |
//! | Python     | `tree-sitter-python`       | Full   |
//! | Dart       | —                          | Stub — no grammar bundled in v0.1 |
//!
//! ## Confidence tiers
//!
//! | Tier | Score range | Source |
//! |------|-------------|--------|
//! | 1    | 1–50        | tree-sitter (this module) |
//! | 2    | 51–90       | compiler / type-checker |
//! | 3    | 100         | federated CAS registry |
//!
//! ## Thread safety
//!
//! `tree_sitter::Parser` is `!Sync`. [`Tier1Indexer`] must be used from a
//! single thread. The daemon wraps indexing in `tokio::task::spawn_blocking`.

pub mod language;
pub mod symbol_extractor;
pub mod tier1;

pub use language::Language;
pub use tier1::Tier1Indexer;
