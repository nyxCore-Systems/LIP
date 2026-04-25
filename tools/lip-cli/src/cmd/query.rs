use std::path::PathBuf;

use clap::{Args, Subcommand};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use lip::query_graph::{ClientMessage, ServerMessage};

use crate::output;

#[derive(Args)]
pub struct QueryArgs {
    /// Path to the daemon Unix socket.
    #[arg(long, default_value = "/tmp/lip-daemon.sock")]
    pub socket: PathBuf,

    #[command(subcommand)]
    pub kind: QueryKind,
}

#[derive(Subcommand)]
pub enum QueryKind {
    /// Find the definition of the symbol at (line, col) in a file.
    Definition { uri: String, line: u32, col: u32 },
    /// Find all references to a symbol URI.
    References {
        symbol_uri: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Show hover info for the symbol at (line, col).
    Hover { uri: String, line: u32, col: u32 },
    /// Compute blast radius for a symbol URI.
    BlastRadius { symbol_uri: String },
    /// Search workspace symbols by name.
    Symbols {
        query: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
    },
    /// List symbols that are defined but never referenced in the workspace.
    DeadSymbols {
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
    /// Fuzzy-search symbol names and docs using trigrams.
    Similar {
        query: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Merkle sync probe: report files whose content hash differs from the daemon's.
    ///
    /// Reads a JSON array of [uri, sha256_hex] pairs from a file or stdin.
    /// Prints the URIs the client should re-send as Delta::Upsert.
    ///
    /// Example input:
    ///   [["file:///src/main.rs", "abc123…"], ["file:///src/lib.rs", "def456…"]]
    StaleFiles {
        /// JSON file containing [[uri, sha256], …] pairs. Omit to read from stdin.
        input: Option<PathBuf>,
    },
    /// Execute multiple queries in one round-trip (reads JSON array from file or stdin).
    ///
    /// Each object in the JSON array must carry a `type` field matching a ClientMessage
    /// variant (snake_case). Example:
    ///
    ///   [{"type":"query_blast_radius","symbol_uri":"lip://local/src/main.rs#main"},
    ///    {"type":"query_references","symbol_uri":"lip://local/src/main.rs#main"},
    ///    {"type":"annotation_get","symbol_uri":"lip://local/src/main.rs#main","key":"lip:fragile"}]
    Batch {
        /// JSON file containing an array of query objects.
        /// Omit to read from stdin.
        input: Option<PathBuf>,
    },
}

pub async fn run(args: QueryArgs) -> anyhow::Result<()> {
    // StaleFiles reads its input before opening the socket.
    if let QueryKind::StaleFiles { ref input } = args.kind {
        let raw = match input {
            Some(path) => tokio::fs::read(path).await?,
            None => {
                use tokio::io::AsyncReadExt as _;
                let mut buf = Vec::new();
                tokio::io::stdin().read_to_end(&mut buf).await?;
                buf
            }
        };
        let files: Vec<(String, String)> = serde_json::from_slice(&raw).map_err(|e| {
            anyhow::anyhow!("input must be a JSON array of [uri, sha256] pairs: {e}")
        })?;
        let msg = ClientMessage::QueryStaleFiles { files };
        let resp = send_recv(&args.socket, msg).await?;
        output::print_json(&resp)?;
        return Ok(());
    }

    // Batch reads its input before opening the socket, so handle it first.
    if let QueryKind::Batch { ref input } = args.kind {
        let raw = match input {
            Some(path) => tokio::fs::read(path).await?,
            None => {
                use tokio::io::AsyncReadExt as _;
                let mut buf = Vec::new();
                tokio::io::stdin().read_to_end(&mut buf).await?;
                buf
            }
        };
        let queries: Vec<ClientMessage> = serde_json::from_slice(&raw).map_err(|e| {
            anyhow::anyhow!("batch input is not a valid JSON array of queries: {e}")
        })?;
        let msg = ClientMessage::BatchQuery { queries };
        let resp = send_recv(&args.socket, msg).await?;
        output::print_json(&resp)?;
        return Ok(());
    }

    let msg = match args.kind {
        QueryKind::Definition { uri, line, col } => {
            ClientMessage::QueryDefinition { uri, line, col }
        }
        QueryKind::References { symbol_uri, limit } => ClientMessage::QueryReferences {
            symbol_uri,
            limit: Some(limit),
        },
        QueryKind::Hover { uri, line, col } => ClientMessage::QueryHover { uri, line, col },
        QueryKind::BlastRadius { symbol_uri } => ClientMessage::QueryBlastRadius { symbol_uri },
        QueryKind::Symbols { query, limit } => ClientMessage::QueryWorkspaceSymbols {
            query,
            limit: Some(limit),
            kind_filter: None,
            scope: None,
            modifier_filter: None,
        },
        QueryKind::DeadSymbols { limit } => ClientMessage::QueryDeadSymbols { limit: Some(limit) },
        QueryKind::Similar { query, limit } => ClientMessage::SimilarSymbols { query, limit },
        QueryKind::StaleFiles { .. } => unreachable!("handled above"),
        QueryKind::Batch { .. } => unreachable!("handled above"),
    };

    let resp = send_recv(&args.socket, msg).await?;
    output::print_json(&resp)?;
    Ok(())
}

async fn send_recv(socket: &PathBuf, msg: ClientMessage) -> anyhow::Result<ServerMessage> {
    let mut stream = UnixStream::connect(socket)
        .await
        .map_err(|e| anyhow::anyhow!("cannot connect to daemon at {}: {e}", socket.display()))?;
    let body = serde_json::to_vec(&msg)?;
    let len = body.len() as u32;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&body).await?;
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let resp_len = u32::from_be_bytes(len_buf) as usize;
    let mut resp_bytes = vec![0u8; resp_len];
    stream.read_exact(&mut resp_bytes).await?;
    Ok(serde_json::from_slice(&resp_bytes)?)
}
