//! # LIP — Linked Incremental Protocol
//!
//! Rust reference implementation of LIP v0.1.
//!
//! LIP is a language-agnostic protocol for streaming, incremental code
//! intelligence. It is designed as a successor to LSP (runtime queries) and
//! SCIP (static snapshots), combining lazy query graphs with a content-addressed
//! dependency registry.
//!
//! ## Quick start
//!
//! ```bash
//! # Start the daemon
//! lip daemon --socket /tmp/lip.sock
//!
//! # Index a directory
//! lip index ./src --output stream.json
//!
//! # Query definition at a position
//! lip query definition file:///src/main.rs 42 10
//!
//! # Start the LSP bridge (connect your editor to this process)
//! lip lsp --socket /tmp/lip.sock
//! ```
//!
//! ## Crate layout
//!
//! | Module | Spec ref | Description |
//! |--------|----------|-------------|
//! | [`schema`] | §2 | Owned types mirroring `schema/lip.fbs`; `LipUri` |
//! | [`query_graph`] | §3.1 | Revision-based incremental query database |
//! | [`indexer`] | §3.3 | Tree-sitter Tier 1 indexer (Rust, TS, Python) |
//! | [`daemon`] | §6, §7.1 | Unix-socket IPC daemon + session loop |
//! | [`bridge`] | §10.1 | LIP-to-LSP bridge (`tower-lsp`) |
//! | [`registry`] | §3.4, §11 | Dependency slice cache + registry HTTP client |
//!
//! ## Confidence tiers
//!
//! | Tier | Score | Source |
//! |------|-------|--------|
//! | 1 | 1–50 | Tree-sitter (this crate, [`indexer`] module) |
//! | 2 | 51–90 | Compiler / type-checker (external, fed in via [`daemon`]) |
//! | 3 | 100 | Federated CAS registry ([`registry`] module) |
//!
//! ## Wire format (v0.1)
//!
//! All IPC uses 4-byte big-endian length-prefixed JSON over a Unix domain
//! socket. The FlatBuffers zero-copy path ([`daemon::mmap`]) is implemented
//! but unused until v0.2.

pub mod bridge;
pub mod daemon;
pub mod indexer;
pub mod query_graph;
pub mod registry;
pub mod schema;
