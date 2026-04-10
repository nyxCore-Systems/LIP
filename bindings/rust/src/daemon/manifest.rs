use crate::schema::IndexingState;
use serde::{Deserialize, Serialize};

/// Sent by the client at connection start (spec §6.2 — Phase 1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestRequest {
    /// Absolute path to the repository root.
    pub repo_root: String,
    /// SHA-256 of the repo's Merkle tree root (spec §3.5).
    pub merkle_root: String,
    /// SHA-256 of the resolved dependency manifest (package.json / Cargo.toml / …).
    pub dep_tree_hash: String,
    /// Protocol version string, e.g. "0.1.0".
    pub lip_version: String,
}

/// Returned by the daemon to the client in response to a ManifestRequest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestResponse {
    /// The Merkle root that the daemon last persisted.
    pub cached_merkle_root: String,
    /// Dependency package hashes not yet in the local slice cache.
    pub missing_slices: Vec<String>,
    /// Current indexing state.
    pub indexing_state: IndexingState,
}
