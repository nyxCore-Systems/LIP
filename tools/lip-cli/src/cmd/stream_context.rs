use std::path::PathBuf;

use clap::Args;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use lip::query_graph::{ClientMessage, ServerMessage};
use lip::schema::OwnedRange;

use crate::output;

#[derive(Args)]
pub struct StreamContextArgs {
    /// Path to the daemon Unix socket.
    #[arg(long, default_value = "/tmp/lip-daemon.sock")]
    pub socket: PathBuf,

    /// File URI to stream context for (e.g. `file:///src/main.rs`).
    pub file_uri: String,

    /// Cursor position as `LINE:COL` (0-based).
    pub position: String,

    /// Maximum estimated prompt-token budget across all streamed symbols.
    #[arg(long, default_value_t = 4096)]
    pub max_tokens: u32,

    /// Optional embedding model override.
    #[arg(long)]
    pub model: Option<String>,
}

pub async fn run(args: StreamContextArgs) -> anyhow::Result<()> {
    let (line, col) = parse_position(&args.position)?;

    let msg = ClientMessage::StreamContext {
        file_uri: args.file_uri,
        cursor_position: OwnedRange {
            start_line: line,
            start_char: col,
            end_line: line,
            end_char: col,
        },
        max_tokens: args.max_tokens,
        model: args.model,
    };

    let mut stream = UnixStream::connect(&args.socket).await.map_err(|e| {
        anyhow::anyhow!("cannot connect to daemon at {}: {e}", args.socket.display())
    })?;
    let body = serde_json::to_vec(&msg)?;
    stream.write_all(&(body.len() as u32).to_be_bytes()).await?;
    stream.write_all(&body).await?;

    loop {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        stream.read_exact(&mut buf).await?;
        let frame: ServerMessage = serde_json::from_slice(&buf)?;
        output::print_json(&frame)?;
        if matches!(frame, ServerMessage::EndStream { .. }) {
            break;
        }
    }
    Ok(())
}

fn parse_position(s: &str) -> anyhow::Result<(i32, i32)> {
    let (l, c) = s
        .split_once(':')
        .ok_or_else(|| anyhow::anyhow!("position must be LINE:COL, got `{s}`"))?;
    let line: i32 = l
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid line `{l}`: {e}"))?;
    let col: i32 = c
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid col `{c}`: {e}"))?;
    Ok((line, col))
}
