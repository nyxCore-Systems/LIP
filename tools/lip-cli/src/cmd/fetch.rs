use std::path::PathBuf;
use std::sync::Arc;

use clap::Args;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use lip::query_graph::{ClientMessage, ServerMessage};
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

    /// After fetching, mount the slice into a running daemon.
    #[arg(long)]
    pub mount: bool,

    /// Daemon socket to mount into (requires --mount).
    #[arg(long, default_value = "/tmp/lip-daemon.sock")]
    pub socket: PathBuf,
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

    if args.mount {
        let mut stream = UnixStream::connect(&args.socket).await.map_err(|e| {
            anyhow::anyhow!("cannot connect to daemon at {}: {e}", args.socket.display())
        })?;
        let msg = ClientMessage::LoadSlice { slice: (*slice).clone() };
        let body = serde_json::to_vec(&msg)?;
        stream.write_all(&(body.len() as u32).to_be_bytes()).await?;
        stream.write_all(&body).await?;

        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let resp_len = u32::from_be_bytes(len_buf) as usize;
        let mut resp_bytes = vec![0u8; resp_len];
        stream.read_exact(&mut resp_bytes).await?;
        let resp: ServerMessage = serde_json::from_slice(&resp_bytes)?;
        output::print_json(&resp)?;
    } else {
        output::print_json(&*slice)?;
    }
    Ok(())
}
