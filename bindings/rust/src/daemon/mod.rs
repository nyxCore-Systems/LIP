//! Unix-socket IPC daemon (spec §6, §7.1).
//!
//! The daemon owns an [`LipDatabase`](crate::query_graph::LipDatabase) and
//! serves multiple clients concurrently over a Unix domain socket. Each
//! accepted connection runs a [`Session`] loop on a dedicated Tokio task.
//!
//! ## Wire framing
//!
//! Messages are length-prefixed JSON:
//!
//! ```text
//! ┌──────────────────┬────────────────────────────────┐
//! │  length : u32 BE │  JSON body (length bytes)      │
//! └──────────────────┴────────────────────────────────┘
//! ```
//!
//! Clients send [`ClientMessage`](crate::query_graph::ClientMessage); the
//! daemon replies with [`ServerMessage`](crate::query_graph::ServerMessage).
//! Both are `serde`-tagged enums (`"type"` field, snake_case variants).
//!
//! ## Connection lifecycle
//!
//! 1. Client sends `Manifest` — the daemon checks its Merkle cache and reports
//!    missing dependency slices.
//! 2. Client streams `Delta` messages as files are edited.
//! 3. Client sends point queries (`QueryDefinition`, `QueryHover`, etc.).
//! 4. Client disconnects (EOF) — session tears down cleanly.
//!
//! ## v0.2 upgrade path
//!
//! [`mmap`] implements a shared-memory region for zero-copy FlatBuffers IPC.
//! In v0.1 it is unused; the daemon will switch to it once the FlatBuffers
//! schema is stabilised.

pub mod journal;
pub mod manifest;
pub mod mmap;
pub mod server;
pub mod session;
pub mod tier2_manager;

pub use journal::{Journal, JournalEntry};
pub use manifest::{ManifestRequest, ManifestResponse};
pub use server::LipDaemon;
pub use session::{read_message, write_client_message, write_message, Session};
pub use tier2_manager::VerificationJob;
