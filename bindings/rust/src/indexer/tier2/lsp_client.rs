//! Minimal LSP JSON-RPC client over a child process's stdin/stdout.
//!
//! The wire format is the standard LSP framing:
//! ```text
//! Content-Length: <N>\r\n
//! \r\n
//! <N bytes of UTF-8 JSON>
//! ```
//!
//! This client is intentionally sequential: each [`LspClient::request`] call
//! sends exactly one request and blocks until its response arrives. Server-
//! initiated requests (`workspace/configuration`, `window/workDoneProgress/create`,
//! etc.) and notifications received while waiting are handled transparently.

use anyhow::Context;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tracing::debug;

pub struct LspClient {
    writer:  ChildStdin,
    reader:  BufReader<ChildStdout>,
    next_id: u64,
}

impl LspClient {
    pub fn new(writer: ChildStdin, stdout: ChildStdout) -> Self {
        Self {
            writer,
            reader:  BufReader::new(stdout),
            next_id: 0,
        }
    }

    /// Send a JSON-RPC request and wait for the matching response.
    ///
    /// While waiting, any server-initiated requests are answered with a
    /// null/empty result and notifications are discarded.
    pub async fn request(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;

        let msg = json!({
            "jsonrpc": "2.0",
            "id":      id,
            "method":  method,
            "params":  params,
        });
        self.write_msg(&msg).await?;

        loop {
            let incoming = self.read_msg().await?;

            // Messages with "method" are server-initiated requests or notifications.
            if let Some(m) = incoming.get("method").and_then(|v| v.as_str()) {
                self.answer_server_msg(&incoming, m).await?;
                continue;
            }

            // Check if this is the response we're waiting for.
            if incoming.get("id").and_then(|v| v.as_u64()) == Some(id) {
                if let Some(err) = incoming.get("error") {
                    anyhow::bail!("LSP error from server: {err}");
                }
                return Ok(incoming.get("result").cloned().unwrap_or(Value::Null));
            }
            // Response to a stale / unrelated id — discard.
        }
    }

    /// Send a JSON-RPC notification (no response expected or awaited).
    pub async fn notify(&mut self, method: &str, params: Value) -> anyhow::Result<()> {
        let msg = json!({"jsonrpc":"2.0","method":method,"params":params});
        self.write_msg(&msg).await
    }

    // ── Internal ──────────────────────────────────────────────────────────────

    async fn answer_server_msg(&mut self, msg: &Value, method: &str) -> anyhow::Result<()> {
        debug!("lsp ← server msg: {method}");
        match method {
            // Server requests that require a null/empty acknowledgement.
            "window/workDoneProgress/create"
            | "client/registerCapability"
            | "client/unregisterCapability"
            | "workspace/applyEdit" => {
                if let Some(id) = msg.get("id") {
                    self.write_msg(&json!({"jsonrpc":"2.0","id":id,"result":null})).await?;
                }
            }
            // rust-analyzer asks for workspace config; return empty objects.
            "workspace/configuration" => {
                if let Some(id) = msg.get("id") {
                    let count = msg.pointer("/params/items")
                        .and_then(|v| v.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0);
                    let empties: Vec<Value> = vec![Value::Object(Default::default()); count];
                    self.write_msg(&json!({"jsonrpc":"2.0","id":id,"result":empties})).await?;
                }
            }
            // All other notifications (publishDiagnostics, logMessage, etc.): discard.
            _ => {}
        }
        Ok(())
    }

    async fn write_msg(&mut self, msg: &Value) -> anyhow::Result<()> {
        let body = serde_json::to_vec(msg)?;
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        self.writer.write_all(header.as_bytes()).await?;
        self.writer.write_all(&body).await?;
        self.writer.flush().await?;
        Ok(())
    }

    async fn read_msg(&mut self) -> anyhow::Result<Value> {
        // Read HTTP-style headers until blank line.
        let mut content_length: Option<usize> = None;
        loop {
            let mut line = String::new();
            self.reader
                .read_line(&mut line)
                .await
                .context("rust-analyzer stdout closed unexpectedly")?;
            let trimmed = line.trim_end_matches(['\r', '\n']);
            if trimmed.is_empty() {
                break;
            }
            if let Some(v) = trimmed.strip_prefix("Content-Length: ") {
                content_length = Some(v.trim().parse().context("bad Content-Length value")?);
            }
        }
        let len = content_length.context("LSP message missing Content-Length header")?;
        let mut body = vec![0u8; len];
        self.reader.read_exact(&mut body).await?;
        serde_json::from_slice(&body).context("LSP message is not valid JSON")
    }
}
