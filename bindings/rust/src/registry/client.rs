use std::sync::Arc;

use tracing::{debug, info};

use crate::schema::{sha256_hex, OwnedDependencySlice};

use super::cache::SliceCache;

/// Registry client — fetches pre-built `DependencySlice`s from the LIP global
/// (or private team) registry (spec §3.4 and §7.2).
///
/// v0.1: plain HTTPS GET. v0.3 will switch to gRPC streaming.
pub struct RegistryClient {
    base_urls: Vec<String>,
    http: reqwest::Client,
    cache: Arc<SliceCache>,
}

impl RegistryClient {
    pub fn new(registry_urls: Vec<String>, cache: Arc<SliceCache>) -> Self {
        let http = reqwest::Client::builder()
            .user_agent(concat!("lip-daemon/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("failed to build HTTP client");
        Self {
            base_urls: registry_urls,
            http,
            cache,
        }
    }

    /// Return a cached slice for `package_hash`, fetching from the registry if needed.
    /// The `content_hash` field on the returned slice has been verified.
    pub async fn fetch_slice(
        &self,
        package_hash: &str,
    ) -> anyhow::Result<Arc<OwnedDependencySlice>> {
        // Fast path: already in local cache.
        if let Some(cached) = self.cache.get(package_hash) {
            debug!("slice cache hit: {package_hash}");
            return Ok(cached);
        }

        // Fetch from the first registry that has it.
        for base in &self.base_urls {
            let url = format!("{base}/slices/{package_hash}");
            debug!("fetching slice from {url}");

            let resp = match self.http.get(&url).send().await {
                Ok(r) if r.status().is_success() => r,
                Ok(r) => {
                    debug!("registry {base} returned {}", r.status());
                    continue;
                }
                Err(e) => {
                    debug!("registry {base} request failed: {e}");
                    continue;
                }
            };

            let blob = resp.bytes().await?;
            let slice: OwnedDependencySlice = serde_json::from_slice(&blob)?;

            // Verify content hash before mounting (spec §11.1).
            let actual = sha256_hex(&blob);
            if actual != slice.content_hash {
                anyhow::bail!(
                    "slice content_hash mismatch from {base}: expected {}, got {actual}",
                    slice.content_hash
                );
            }

            info!(
                "fetched slice {}/{}@{} ({} bytes)",
                slice.manager,
                slice.package_name,
                slice.version,
                blob.len()
            );
            self.cache.insert(slice.clone())?;
            return Ok(Arc::new(slice));
        }

        anyhow::bail!("slice {package_hash} not found in any registry")
    }

    /// Publish a raw slice JSON blob to the first registry.
    ///
    /// The server verifies the SHA-256 of the body matches the URL path and the
    /// `content_hash` field inside the JSON. Returns the content hash on success.
    pub async fn push_slice(&self, raw: Vec<u8>) -> anyhow::Result<String> {
        let hash = sha256_hex(&raw);
        let base = self
            .base_urls
            .first()
            .ok_or_else(|| anyhow::anyhow!("no registry URL configured"))?;
        let url = format!("{base}/slices/{hash}");

        let resp = self
            .http
            .put(&url)
            .header("Content-Type", "application/json")
            .body(raw)
            .send()
            .await?;

        anyhow::ensure!(
            resp.status().is_success(),
            "registry rejected push ({}): {}",
            resp.status(),
            resp.text().await.unwrap_or_default()
        );

        info!("pushed slice {hash} to {base}");
        Ok(hash)
    }
}
