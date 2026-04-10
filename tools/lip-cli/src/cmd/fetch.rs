use std::path::PathBuf;
use std::sync::Arc;

use clap::Args;

use lip::registry::{cache::SliceCache, client::RegistryClient};

use crate::output;

#[derive(Args)]
pub struct FetchArgs {
    /// SHA-256 content hash of the dependency slice to fetch.
    pub package_hash: String,

    /// Registry base URL(s). May be specified multiple times.
    #[arg(long = "registry", default_value = "https://registry.lip.dev")]
    pub registries: Vec<String>,

    /// Local slice cache directory.
    #[arg(long, default_value = "~/.cache/lip/slices")]
    pub cache_dir: PathBuf,
}

pub async fn run(args: FetchArgs) -> anyhow::Result<()> {
    // Expand `~` in the cache path.
    let cache_dir = if args.cache_dir.starts_with("~") {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(args.cache_dir.to_string_lossy().replacen('~', &home, 1))
    } else {
        args.cache_dir
    };

    let cache  = Arc::new(SliceCache::open(&cache_dir)?);
    let client = RegistryClient::new(args.registries, cache);
    let slice  = client.fetch_slice(&args.package_hash).await?;

    output::print_json(&*slice)?;
    Ok(())
}
