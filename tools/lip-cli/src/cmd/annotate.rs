use std::path::PathBuf;

use clap::{Args, Subcommand};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use lip::query_graph::{ClientMessage, ServerMessage};

use crate::output;

#[derive(Args)]
pub struct AnnotateArgs {
    /// Path to the daemon Unix socket.
    #[arg(long, default_value = "/tmp/lip-daemon.sock")]
    pub socket: PathBuf,

    #[command(subcommand)]
    pub kind: AnnotateKind,
}

#[derive(Subcommand)]
pub enum AnnotateKind {
    /// Set or overwrite an annotation on a symbol.
    Set {
        symbol_uri: String,
        key:        String,
        value:      String,
        /// Identifier for the author; defaults to "human:cli".
        #[arg(long, default_value = "human:cli")]
        author: String,
    },
    /// Get an annotation value for a (symbol, key) pair.
    Get {
        symbol_uri: String,
        key:        String,
    },
    /// List all annotations for a symbol.
    List {
        symbol_uri: String,
    },
}

pub async fn run(args: AnnotateArgs) -> anyhow::Result<()> {
    let mut stream = UnixStream::connect(&args.socket).await.map_err(|e| {
        anyhow::anyhow!("cannot connect to daemon at {}: {e}", args.socket.display())
    })?;

    let msg = match args.kind {
        AnnotateKind::Set { symbol_uri, key, value, author } => {
            ClientMessage::AnnotationSet { symbol_uri, key, value, author_id: author }
        }
        AnnotateKind::Get { symbol_uri, key } => {
            ClientMessage::AnnotationGet { symbol_uri, key }
        }
        AnnotateKind::List { symbol_uri } => {
            ClientMessage::AnnotationList { symbol_uri }
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
