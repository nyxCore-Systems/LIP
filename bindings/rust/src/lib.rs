//! # LIP — Linked Incremental Protocol
//!
//! Rust reference implementation of LIP v1.3.
//!
//! LIP is a language-agnostic protocol for streaming, incremental code
//! intelligence. It is designed as a successor to LSP (runtime queries) and
//! SCIP (static snapshots), combining lazy query graphs with a content-addressed
//! dependency registry.
//!
//! ## Quick start
//!
//! ```bash
//! # Start the daemon (watches files, persists to WAL journal)
//! lip daemon --socket /tmp/lip.sock
//!
//! # Index a directory (Tier 1, tree-sitter)
//! lip index ./src
//!
//! # Query definition at a position
//! lip query definition file:///src/main.rs 42 10
//!
//! # Start the LSP bridge (standard LSP, no editor plugin needed)
//! lip lsp --socket /tmp/lip.sock
//!
//! # Start the MCP server (AI agents: Claude Code, CKB, Cursor, …)
//! lip mcp --socket /tmp/lip.sock
//!
//! # Semantic nearest-neighbour search (requires LIP_EMBEDDING_URL)
//! lip query embedding-batch file:///src/auth.rs
//! lip query nearest         file:///src/auth.rs --top-k 5
//! ```
//!
//! ## Crate layout
//!
//! | Module | Spec ref | Description |
//! |--------|----------|-------------|
//! | [`schema`] | §2 | Owned types mirroring `schema/lip.fbs`; `LipUri` |
//! | [`query_graph`] | §3.1 | Revision-based incremental query database |
//! | [`indexer`] | §3.3 | Tree-sitter Tier 1 indexer (Rust, TS, Python, Dart) |
//! | [`daemon`] | §6, §7.1 | Unix-socket IPC daemon + session loop |
//! | [`daemon::embedding`] | — | OpenAI-compatible HTTP embedding client |
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
//! ## Semantic embeddings (v1.3)
//!
//! When `LIP_EMBEDDING_URL` points to an OpenAI-compatible endpoint (e.g. Ollama,
//! OpenAI, Together AI), the daemon can store dense embedding vectors per file and
//! answer cosine nearest-neighbour queries:
//!
//! ```text
//! LIP_EMBEDDING_URL=http://localhost:11434/v1/embeddings
//! LIP_EMBEDDING_MODEL=nomic-embed-text   # optional; defaults to text-embedding-3-small
//! ```
//!
//! Use `ClientMessage::EmbeddingBatch` to populate vectors, then
//! `ClientMessage::QueryNearest` or `ClientMessage::QueryNearestByText` to search.
//! Embedding is optional — all other LIP features work without it.
//!
//! ## Wire format
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
