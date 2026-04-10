use std::path::PathBuf;
use std::sync::Arc;

use clap::Args;
use tokio::io::AsyncReadExt;

use lip::registry::{cache::SliceCache, client::RegistryClient};
use lip::schema::OwnedDependencySlice;

#[derive(Args)]
pub struct PushArgs {
    /// Slice JSON file to publish (omit to read from stdin).
    pub slice_file: Option<PathBuf>,

    /// Registry base URL to publish to.
    #[arg(long = "registry", default_value = "https://registry.lip.dev")]
    pub registry: String,

    /// Local slice cache directory.
    #[arg(long, default_value = "~/.cache/lip/slices")]
    pub cache_dir: PathBuf,
}

pub async fn run(args: PushArgs) -> anyhow::Result<()> {
    let raw: Vec<u8> = match args.slice_file {
        Some(p) => tokio::fs::read(&p).await?,
        None    => {
            let mut buf = Vec::new();
            tokio::io::stdin().read_to_end(&mut buf).await?;
            buf
        }
    };

    // Validate before sending so errors are local and clear.
    let _slice: OwnedDependencySlice = serde_json::from_slice(&raw)
        .map_err(|e| anyhow::anyhow!("input is not a valid OwnedDependencySlice: {e}"))?;

    let cache_dir = if args.cache_dir.starts_with("~") {
        let home = std::env::var("HOME").unwrap_or_default();
        PathBuf::from(args.cache_dir.to_string_lossy().replacen('~', &home, 1))
    } else {
        args.cache_dir
    };

    let cache  = Arc::new(SliceCache::open(&cache_dir)?);
    let client = RegistryClient::new(vec![args.registry], cache);
    let hash   = client.push_slice(raw).await?;

    println!("{hash}");
    Ok(())
}
