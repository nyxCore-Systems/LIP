use std::path::{Path, PathBuf};
use std::sync::Arc;

use dashmap::DashMap;
use tracing::{debug, warn};

use crate::schema::{sha256_hex, OwnedDependencySlice};

/// Local content-addressable cache for dependency slices (spec §3.4).
///
/// Slices are keyed by `content_hash` (SHA-256 of the blob). Once a slice for
/// `react@18.2.0` is cached, it is never re-downloaded on any machine.
pub struct SliceCache {
    dir:   PathBuf,
    index: DashMap<String, Arc<OwnedDependencySlice>>,
}

impl SliceCache {
    /// Open (or create) the cache directory at `dir`.
    pub fn open(dir: impl AsRef<Path>) -> anyhow::Result<Self> {
        let dir = dir.as_ref().to_owned();
        std::fs::create_dir_all(&dir)?;
        let cache = Self { dir, index: DashMap::new() };
        cache.load_from_disk()?;
        Ok(cache)
    }

    /// Look up a cached slice by its `content_hash`.
    pub fn get(&self, content_hash: &str) -> Option<Arc<OwnedDependencySlice>> {
        self.index.get(content_hash).map(|v| v.value().clone())
    }

    /// Insert a slice into the in-memory index and persist it to disk.
    pub fn insert(&self, slice: OwnedDependencySlice) -> anyhow::Result<()> {
        let hash = slice.content_hash.clone();
        let blob = serde_json::to_vec(&slice)?;
        self.verify_hash(&blob, &hash)?;
        let path = self.slice_path(&hash);
        std::fs::write(&path, &blob)?;
        debug!("cached slice {} ({} bytes)", hash, blob.len());
        self.index.insert(hash, Arc::new(slice));
        Ok(())
    }

    fn slice_path(&self, content_hash: &str) -> PathBuf {
        self.dir.join(format!("{content_hash}.slice.json"))
    }

    fn verify_hash(&self, blob: &[u8], expected: &str) -> anyhow::Result<()> {
        let actual = sha256_hex(blob);
        if actual != expected {
            anyhow::bail!(
                "slice hash mismatch: expected {expected}, got {actual}"
            );
        }
        Ok(())
    }

    fn load_from_disk(&self) -> anyhow::Result<()> {
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let path  = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                match self.load_slice(&path) {
                    Ok(slice) => {
                        self.index.insert(slice.content_hash.clone(), Arc::new(slice));
                    }
                    Err(e) => warn!("failed to load cached slice {:?}: {e}", path),
                }
            }
        }
        Ok(())
    }

    fn load_slice(&self, path: &Path) -> anyhow::Result<OwnedDependencySlice> {
        let blob  = std::fs::read(path)?;
        let slice: OwnedDependencySlice = serde_json::from_slice(&blob)?;
        Ok(slice)
    }
}
