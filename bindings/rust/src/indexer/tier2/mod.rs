//! Tier 2 compiler-backed indexer (spec §3.3, confidence 51–90).
//!
//! Ships three LSP backends:
//! - [`rust_analyzer`]: `rust-analyzer` for Rust files
//! - [`ts_server`]: `typescript-language-server` for TypeScript/TSX files
//! - [`py_ls`]: `pyright-langserver` (or `pylsp` fallback) for Python files
//!
//! # Integration
//!
//! The [`Tier2Manager`](crate::daemon::tier2_manager::Tier2Manager) runs as a
//! background tokio task. When a session receives a supported-language `Delta`,
//! it pushes a [`VerificationJob`] to the manager's channel. The manager routes
//! the job to the appropriate backend and writes upgraded symbols back into the
//! `LipDatabase` via
//! [`LipDatabase::upgrade_file_symbols`](crate::query_graph::LipDatabase::upgrade_file_symbols).
//!
//! # Graceful degradation
//!
//! If a language server binary is not in `PATH`, the manager logs a warning and
//! permanently disables that language's Tier 2 work for the session. Tier 1
//! results remain fully functional.

pub mod lsp_client;
pub mod rust_analyzer;
pub mod ts_server;
pub mod py_ls;

pub use rust_analyzer::VerificationResult;
pub use ts_server::TypeScriptBackend;
pub use py_ls::PythonBackend;
