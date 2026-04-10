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
    Definition {
        uri:  String,
        line: u32,
        col:  u32,
    },
    /// Find all references to a symbol URI.
    References {
        symbol_uri: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Show hover info for the symbol at (line, col).
    Hover {
        uri:  String,
        line: u32,
        col:  u32,
    },
    /// Compute blast radius for a symbol URI.
    BlastRadius {
        symbol_uri: String,
    },
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
}

pub async fn run(args: QueryArgs) -> anyhow::Result<()> {
    let mut stream = UnixStream::connect(&args.socket).await.map_err(|e| {
        anyhow::anyhow!("cannot connect to daemon at {}: {e}", args.socket.display())
    })?;

    let msg = match args.kind {
        QueryKind::Definition { uri, line, col } => {
            ClientMessage::QueryDefinition { uri, line, col }
        }
        QueryKind::References { symbol_uri, limit } => {
            ClientMessage::QueryReferences { symbol_uri, limit: Some(limit) }
        }
        QueryKind::Hover { uri, line, col } => {
            ClientMessage::QueryHover { uri, line, col }
        }
        QueryKind::BlastRadius { symbol_uri } => {
            ClientMessage::QueryBlastRadius { symbol_uri }
        }
        QueryKind::Symbols { query, limit } => {
            ClientMessage::QueryWorkspaceSymbols { query, limit: Some(limit) }
        }
        QueryKind::DeadSymbols { limit } => {
            ClientMessage::QueryDeadSymbols { limit: Some(limit) }
        }
    };

    let body = serde_json::to_vec(&msg)?;
    let len  = body.len() as u32;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&body).await?;

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let resp_len = u32::from_be_bytes(len_buf) as usize;
    let mut resp_bytes = vec![0u8; resp_len];
    stream.read_exact(&mut resp_bytes).await?;

    let resp: ServerMessage = serde_json::from_slice(&resp_bytes)?;
    output::print_json(&resp)?;
    Ok(())
}
