//! Dependency slice cache and registry client (spec §3.4, §11).
//!
//! LIP avoids re-indexing third-party dependencies by distributing
//! pre-built *dependency slices* — content-addressed JSON blobs that carry
//! the full symbol table for one package version.
//!
//! ## Fetch flow
//!
//! ```text
//! lip fetch <hash>
//!      │
//!      ▼
//! SliceCache::get(hash)  ── hit ──► return Arc<OwnedDependencySlice>
//!      │ miss
//!      ▼
//! RegistryClient::fetch_slice(hash)
//!      │  HTTPS GET /slices/<hash>
//!      ▼
//! verify content_hash (SHA-256)   ◄── spec §11.1 — reject on mismatch
//!      │
//!      ▼
//! SliceCache::insert  ──► persist to ~/.cache/lip/slices/<hash>.slice.json
//! ```
//!
//! ## Content addressing
//!
//! Slices are keyed by the SHA-256 of their serialised JSON body (`content_hash`
//! field). This means a cached slice for `react@18.2.0` is never re-downloaded
//! on any machine that has seen it before, regardless of which registry URL
//! served it.
//!
//! ## v0.3 upgrade path
//!
//! The registry HTTP API will switch to gRPC streaming in v0.3, allowing
//! partial slice streaming and delta updates.

pub mod cache;
pub mod client;

pub use cache::SliceCache;
pub use client::RegistryClient;
