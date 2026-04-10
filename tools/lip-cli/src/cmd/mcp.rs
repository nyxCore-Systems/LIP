//! `lip mcp` — MCP server that exposes the LIP daemon as Model Context Protocol tools.
//!
//! Speaks JSON-RPC 2.0 over stdio (newline-delimited). Add to your MCP client config:
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "lip": {
//!       "command": "lip",
//!       "args": ["mcp", "--socket", "/tmp/lip-daemon.sock"]
//!     }
//!   }
//! }
//! ```

use std::path::{Path, PathBuf};

use clap::Args;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use lip::query_graph::{ClientMessage, ServerMessage};

/// Start a Model Context Protocol server that exposes LIP queries as MCP tools.
///
/// Reads JSON-RPC 2.0 from stdin, writes responses to stdout.
/// The LIP daemon must already be running on `--socket`.
#[derive(Args)]
pub struct McpArgs {
    /// Path to the LIP daemon Unix socket.
    #[arg(long, default_value = "/tmp/lip-daemon.sock")]
    pub socket: PathBuf,
}

pub async fn run(args: McpArgs) -> anyhow::Result<()> {
    let mut lines  = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::BufWriter::new(tokio::io::stdout());

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_owned();
        if line.is_empty() { continue; }

        let msg: Value = match serde_json::from_str(&line) {
            Ok(v)  => v,
            Err(_) => continue,
        };

        // Notifications carry no "id" — no response required.
        let id = match msg.get("id") {
            Some(id) => id.clone(),
            None     => continue,
        };

        let method = msg["method"].as_str().unwrap_or("").to_owned();
        let result = dispatch(&method, &msg["params"], &args.socket).await;

        let response = match result {
            Ok(r)  => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err(e) => json!({
                "jsonrpc": "2.0",
                "id":      id,
                "error":   { "code": -32603, "message": e.to_string() }
            }),
        };

        let mut out = serde_json::to_string(&response)?;
        out.push('\n');
        stdout.write_all(out.as_bytes()).await?;
        stdout.flush().await?;
    }

    Ok(())
}

// ── Method dispatch ───────────────────────────────────────────────────────────

async fn dispatch(method: &str, params: &Value, socket: &Path) -> anyhow::Result<Value> {
    match method {
        "initialize" => Ok(json!({
            "protocolVersion": "2024-11-05",
            "serverInfo": {
                "name":    "lip-mcp",
                "version": env!("CARGO_PKG_VERSION")
            },
            "capabilities": { "tools": {} }
        })),

        "tools/list" => Ok(json!({ "tools": tools_manifest() })),

        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("");
            let srv  = daemon_call(name, &params["arguments"], socket).await?;
            let text = format_response(name, &srv);
            Ok(json!({ "content": [{ "type": "text", "text": text }] }))
        }

        other => anyhow::bail!("unsupported method: {other}"),
    }
}

// ── Tool call → ClientMessage ─────────────────────────────────────────────────

async fn daemon_call(name: &str, args: &Value, socket: &Path) -> anyhow::Result<ServerMessage> {
    let msg = match name {
        "lip_blast_radius" => ClientMessage::QueryBlastRadius {
            symbol_uri: req_str(args, "symbol_uri")?,
        },
        "lip_workspace_symbols" => ClientMessage::QueryWorkspaceSymbols {
            query: req_str(args, "query")?,
            limit: args["limit"].as_u64().map(|n| n as usize).or(Some(50)),
        },
        "lip_definition" => ClientMessage::QueryDefinition {
            uri:  req_str(args, "uri")?,
            line: req_u32(args, "line")?,
            col:  req_u32(args, "col")?,
        },
        "lip_references" => ClientMessage::QueryReferences {
            symbol_uri: req_str(args, "symbol_uri")?,
            limit: args["limit"].as_u64().map(|n| n as usize).or(Some(50)),
        },
        "lip_hover" => ClientMessage::QueryHover {
            uri:  req_str(args, "uri")?,
            line: req_u32(args, "line")?,
            col:  req_u32(args, "col")?,
        },
        "lip_document_symbols" => ClientMessage::QueryDocumentSymbols {
            uri: req_str(args, "uri")?,
        },
        "lip_dead_symbols" => ClientMessage::QueryDeadSymbols {
            limit: args["limit"].as_u64().map(|n| n as usize).or(Some(50)),
        },
        "lip_annotation_get" => ClientMessage::AnnotationGet {
            symbol_uri: req_str(args, "symbol_uri")?,
            key:        req_str(args, "key")?,
        },
        "lip_annotation_set" => ClientMessage::AnnotationSet {
            symbol_uri: req_str(args, "symbol_uri")?,
            key:        req_str(args, "key")?,
            value:      req_str(args, "value")?,
            author_id:  req_str(args, "author_id")?,
        },
        other => anyhow::bail!("unknown LIP tool: {other}"),
    };

    query_daemon(socket, msg).await
}

// ── ServerMessage → human-readable text ──────────────────────────────────────

fn format_response(tool: &str, msg: &ServerMessage) -> String {
    match msg {
        ServerMessage::BlastRadiusResult(r) => {
            let files = r.affected_files.join("\n  ");
            format!(
                "Blast radius for `{}`:\n\
                 direct dependents:     {}\n\
                 transitive dependents: {}\n\
                 affected files ({}):\n  {}",
                r.symbol_uri,
                r.direct_dependents,
                r.transitive_dependents,
                r.affected_files.len(),
                if files.is_empty() { "(none)".into() } else { files },
            )
        }
        ServerMessage::WorkspaceSymbolsResult { symbols } => {
            if symbols.is_empty() {
                return "No symbols found.".into();
            }
            symbols.iter()
                .map(|s| format!("{:<30} {:<12}  {}", s.display_name, format!("{:?}", s.kind), s.uri))
                .collect::<Vec<_>>()
                .join("\n")
        }
        ServerMessage::DefinitionResult { symbol, location_uri, location_range } => {
            match (symbol, location_uri) {
                (Some(sym), Some(uri)) => {
                    let pos = location_range.as_ref()
                        .map(|r| format!("{}:{}", r.start_line + 1, r.start_char + 1))
                        .unwrap_or_default();
                    let sig = sym.signature.as_deref().unwrap_or(&sym.display_name);
                    format!("{uri}:{pos}\n```\n{sig}\n```")
                }
                _ => "Definition not found.".into(),
            }
        }
        ServerMessage::ReferencesResult { occurrences } => {
            if occurrences.is_empty() {
                return "No references found.".into();
            }
            occurrences.iter()
                .map(|o| format!("{}  line {}", o.symbol_uri, o.range.start_line + 1))
                .collect::<Vec<_>>()
                .join("\n")
        }
        ServerMessage::HoverResult { symbol } => match symbol {
            Some(s) => {
                let sig  = s.signature.as_deref().unwrap_or(&s.display_name);
                let docs = s.documentation.as_deref().unwrap_or("").trim();
                if docs.is_empty() {
                    format!("```\n{sig}\n```")
                } else {
                    format!("```\n{sig}\n```\n\n{docs}")
                }
            }
            None => "No hover information available.".into(),
        },
        ServerMessage::DocumentSymbolsResult { symbols }
        | ServerMessage::DeadSymbolsResult   { symbols } => {
            if symbols.is_empty() {
                return if tool == "lip_dead_symbols" {
                    "No dead symbols found.".into()
                } else {
                    "No symbols in file.".into()
                };
            }
            symbols.iter()
                .map(|s| format!("{:<30} {:?}", s.display_name, s.kind))
                .collect::<Vec<_>>()
                .join("\n")
        }
        ServerMessage::AnnotationAck => "Annotation saved.".into(),
        ServerMessage::AnnotationValue { value } => {
            value.clone().unwrap_or_else(|| "(not set)".into())
        }
        ServerMessage::Error { message } => format!("LIP error: {message}"),
        // Catch-all: emit JSON so nothing is silently lost.
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    }
}

// ── Daemon IPC (one connection per call — simple and drift-free) ──────────────

async fn query_daemon(socket: &Path, msg: ClientMessage) -> anyhow::Result<ServerMessage> {
    let mut stream = UnixStream::connect(socket).await.map_err(|e| {
        anyhow::anyhow!(
            "cannot connect to LIP daemon at {}: {e}\n\
             Start the daemon first:  lip daemon --socket {}",
            socket.display(),
            socket.display(),
        )
    })?;

    let body = serde_json::to_vec(&msg)?;
    let len  = body.len() as u32;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(&body).await?;

    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let resp_len = u32::from_be_bytes(len_buf) as usize;
    let mut resp_bytes = vec![0u8; resp_len];
    stream.read_exact(&mut resp_bytes).await?;

    Ok(serde_json::from_slice(&resp_bytes)?)
}

// ── Argument helpers ──────────────────────────────────────────────────────────

fn req_str(args: &Value, key: &str) -> anyhow::Result<String> {
    args[key].as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("missing required argument `{key}`"))
}

fn req_u32(args: &Value, key: &str) -> anyhow::Result<u32> {
    args[key].as_u64()
        .map(|n| n as u32)
        .ok_or_else(|| anyhow::anyhow!("missing required argument `{key}`"))
}

// ── MCP tool manifest ─────────────────────────────────────────────────────────

fn tools_manifest() -> Value {
    json!([
        {
            "name": "lip_blast_radius",
            "description": "Analyze the blast radius of a symbol — which files are transitively \
                            affected if this symbol changes. Call BEFORE modifying any function, \
                            class, or interface.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol_uri": {
                        "type": "string",
                        "description": "LIP symbol URI, e.g. lip://local/src/auth.rs#verifyToken"
                    }
                },
                "required": ["symbol_uri"]
            }
        },
        {
            "name": "lip_workspace_symbols",
            "description": "Search for symbols by name across the entire workspace. \
                            Faster and more precise than grep — returns kind, location, \
                            and confidence for each match.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "default": 50 }
                },
                "required": ["query"]
            }
        },
        {
            "name": "lip_definition",
            "description": "Find the definition of the symbol at a given (line, col) in a file.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uri":  { "type": "string",  "description": "file:///absolute/path.rs" },
                    "line": { "type": "integer", "description": "0-based line number" },
                    "col":  { "type": "integer", "description": "0-based UTF-8 byte offset" }
                },
                "required": ["uri", "line", "col"]
            }
        },
        {
            "name": "lip_references",
            "description": "Find all references to a symbol URI across the workspace.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol_uri": { "type": "string" },
                    "limit":      { "type": "integer", "default": 50 }
                },
                "required": ["symbol_uri"]
            }
        },
        {
            "name": "lip_hover",
            "description": "Get type signature and documentation for the symbol at (line, col).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uri":  { "type": "string"  },
                    "line": { "type": "integer" },
                    "col":  { "type": "integer" }
                },
                "required": ["uri", "line", "col"]
            }
        },
        {
            "name": "lip_document_symbols",
            "description": "List all symbols (functions, structs, classes, types) defined in a file.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uri": { "type": "string" }
                },
                "required": ["uri"]
            }
        },
        {
            "name": "lip_dead_symbols",
            "description": "Find symbols that are defined but never referenced — dead code candidates.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "default": 50 }
                }
            }
        },
        {
            "name": "lip_annotation_get",
            "description": "Get a persistent annotation on a symbol (e.g. owner, fragility notes). \
                            Annotations survive daemon restarts and file changes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol_uri": { "type": "string" },
                    "key": {
                        "type": "string",
                        "description": "e.g. 'team:owner', 'lip:fragile', 'agent:note'"
                    }
                },
                "required": ["symbol_uri", "key"]
            }
        },
        {
            "name": "lip_annotation_set",
            "description": "Attach a persistent annotation to a symbol — ownership, fragility \
                            warnings, agent notes. Survives daemon restarts and file changes.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "symbol_uri": { "type": "string" },
                    "key": {
                        "type": "string",
                        "description": "e.g. 'team:owner', 'lip:fragile', 'agent:note'"
                    },
                    "value":     { "type": "string" },
                    "author_id": {
                        "type": "string",
                        "description": "e.g. 'agent:claude' or 'human:alice'"
                    }
                },
                "required": ["symbol_uri", "key", "value", "author_id"]
            }
        }
    ])
}
