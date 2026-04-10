//! LIP-to-LSP bridge (spec §10.1).
//!
//! [`LipLspBackend`] implements [`tower_lsp::LanguageServer`] and acts as a
//! thin adapter between any LSP-speaking editor and a running LIP daemon.
//!
//! ## Architecture
//!
//! ```text
//!  Editor (LSP client)
//!       │  stdin / stdout  (JSON-RPC 2.0)
//!       ▼
//!  LipLspBackend   ◄── implements tower_lsp::LanguageServer
//!       │  Unix socket  (length-prefixed JSON)
//!       ▼
//!  LipDaemon
//! ```
//!
//! The bridge maintains a single persistent socket connection to the daemon
//! and reuses it across requests (`Arc<Mutex<Option<UnixStream>>>`). If the
//! connection drops it is re-established on the next request.
//!
//! ## LSP capabilities advertised (v0.1)
//!
//! - `textDocument/definition`
//! - `textDocument/references`
//! - `textDocument/hover`
//! - `workspace/symbol`
//! - `textDocument/didOpen` / `textDocument/didChange`
//!
//! ## `translate` module
//!
//! [`translate`] converts LIP types to their `lsp_types` equivalents.
//! All LSP types are imported from `tower_lsp::lsp_types` to avoid version
//! conflicts with a standalone `lsp-types` dependency.

pub mod lsp_server;
pub mod translate;

pub use lsp_server::LipLspBackend;
