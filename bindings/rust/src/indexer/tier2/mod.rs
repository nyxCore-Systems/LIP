//! Tier 2 compiler-backed indexer (spec §3.3, confidence 51–90).
//!
//! Currently ships one backend: [`rust_analyzer`], which speaks the Language
//! Server Protocol to a `rust-analyzer` subprocess.
//!
//! # Integration
//!
//! The [`Tier2Manager`](crate::daemon::tier2_manager::Tier2Manager) runs as a
//! background tokio task. When a session receives a Rust-file `Delta`, it
//! pushes a [`VerificationJob`] to the manager's channel. The manager calls
//! the backend and writes upgraded symbols back into the `LipDatabase` via
//! [`LipDatabase::upgrade_file_symbols`](crate::query_graph::LipDatabase::upgrade_file_symbols).
//!
//! # Graceful degradation
//!
//! If `rust-analyzer` is not in `PATH`, the manager logs a warning and skips
//! all Tier 2 work. Tier 1 results remain fully functional.

pub mod lsp_client;
pub mod rust_analyzer;

pub use rust_analyzer::VerificationResult;
