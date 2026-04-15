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
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut stdout = tokio::io::BufWriter::new(tokio::io::stdout());

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_owned();
        if line.is_empty() {
            continue;
        }

        let msg: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Notifications carry no "id" — no response required.
        let id = match msg.get("id") {
            Some(id) => id.clone(),
            None => continue,
        };

        let method = msg["method"].as_str().unwrap_or("").to_owned();
        let result = dispatch(&method, &msg["params"], &args.socket).await;

        let response = match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
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
            let srv = daemon_call(name, &params["arguments"], socket).await?;
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
            uri: req_str(args, "uri")?,
            line: req_u32(args, "line")?,
            col: req_u32(args, "col")?,
        },
        "lip_references" => ClientMessage::QueryReferences {
            symbol_uri: req_str(args, "symbol_uri")?,
            limit: args["limit"].as_u64().map(|n| n as usize).or(Some(50)),
        },
        "lip_hover" => ClientMessage::QueryHover {
            uri: req_str(args, "uri")?,
            line: req_u32(args, "line")?,
            col: req_u32(args, "col")?,
        },
        "lip_document_symbols" => ClientMessage::QueryDocumentSymbols {
            uri: req_str(args, "uri")?,
        },
        "lip_dead_symbols" => ClientMessage::QueryDeadSymbols {
            limit: args["limit"].as_u64().map(|n| n as usize).or(Some(50)),
        },
        "lip_annotation_get" => ClientMessage::AnnotationGet {
            symbol_uri: req_str(args, "symbol_uri")?,
            key: req_str(args, "key")?,
        },
        "lip_annotation_set" => ClientMessage::AnnotationSet {
            symbol_uri: req_str(args, "symbol_uri")?,
            key: req_str(args, "key")?,
            value: req_str(args, "value")?,
            author_id: req_str(args, "author_id")?,
        },
        "lip_batch_query" => {
            let queries_val = args
                .get("queries")
                .ok_or_else(|| anyhow::anyhow!("missing required argument `queries`"))?;
            let queries: Vec<ClientMessage> =
                serde_json::from_value(queries_val.clone()).map_err(|e| {
                    anyhow::anyhow!("queries is not a valid array of query objects: {e}")
                })?;
            ClientMessage::BatchQuery { queries }
        }
        "lip_similar_symbols" => ClientMessage::SimilarSymbols {
            query: req_str(args, "query")?,
            limit: args["limit"].as_u64().map(|n| n as usize).unwrap_or(20),
        },
        "lip_annotation_workspace_list" => ClientMessage::AnnotationWorkspaceList {
            key_prefix: args["key_prefix"].as_str().unwrap_or("").to_owned(),
        },
        "lip_stale_files" => {
            let files_val = args
                .get("files")
                .ok_or_else(|| anyhow::anyhow!("missing required argument `files`"))?;
            let files: Vec<(String, String)> =
                serde_json::from_value(files_val.clone()).map_err(|e| {
                    anyhow::anyhow!("`files` must be an array of [uri, sha256] pairs: {e}")
                })?;
            ClientMessage::QueryStaleFiles { files }
        }
        "lip_load_slice" => {
            let slice_val = args
                .get("slice")
                .ok_or_else(|| anyhow::anyhow!("missing required argument `slice`"))?;
            let slice = serde_json::from_value(slice_val.clone()).map_err(|e| {
                anyhow::anyhow!("`slice` must be an OwnedDependencySlice JSON object: {e}")
            })?;
            ClientMessage::LoadSlice { slice }
        }
        "lip_embedding_batch" => {
            let uris_val = args
                .get("uris")
                .ok_or_else(|| anyhow::anyhow!("missing required argument `uris`"))?;
            let uris: Vec<String> = serde_json::from_value(uris_val.clone())
                .map_err(|e| anyhow::anyhow!("`uris` must be an array of strings: {e}"))?;
            ClientMessage::EmbeddingBatch {
                uris,
                model: args["model"].as_str().map(str::to_owned),
            }
        }
        "lip_index_status" => ClientMessage::QueryIndexStatus,
        "lip_file_status" => ClientMessage::QueryFileStatus {
            uri: req_str(args, "uri")?,
        },
        "lip_nearest" => ClientMessage::QueryNearest {
            uri: req_str(args, "uri")?,
            top_k: args["top_k"].as_u64().map(|n| n as usize).unwrap_or(10),
            filter: args["filter"].as_str().map(str::to_owned),
            min_score: args["min_score"].as_f64().map(|f| f as f32),
        },
        "lip_nearest_by_text" => ClientMessage::QueryNearestByText {
            text: req_str(args, "text")?,
            top_k: args["top_k"].as_u64().map(|n| n as usize).unwrap_or(10),
            model: args["model"].as_str().map(str::to_owned),
            filter: args["filter"].as_str().map(str::to_owned),
            min_score: args["min_score"].as_f64().map(|f| f as f32),
        },
        "lip_nearest_by_contrast" => ClientMessage::QueryNearestByContrast {
            like_uri: req_str(args, "like_uri")?,
            unlike_uri: req_str(args, "unlike_uri")?,
            top_k: args["top_k"].as_u64().map(|n| n as usize).unwrap_or(10),
            filter: args["filter"].as_str().map(str::to_owned),
            min_score: args["min_score"].as_f64().map(|f| f as f32),
        },
        "lip_outliers" => {
            let uris_val = args
                .get("uris")
                .ok_or_else(|| anyhow::anyhow!("missing required argument `uris`"))?;
            let uris: Vec<String> = serde_json::from_value(uris_val.clone())
                .map_err(|e| anyhow::anyhow!("`uris` must be an array of strings: {e}"))?;
            ClientMessage::QueryOutliers {
                uris,
                top_k: args["top_k"].as_u64().map(|n| n as usize).unwrap_or(5),
            }
        }
        "lip_semantic_drift" => ClientMessage::QuerySemanticDrift {
            uri_a: req_str(args, "uri_a")?,
            uri_b: req_str(args, "uri_b")?,
        },
        "lip_similarity_matrix" => {
            let uris_val = args
                .get("uris")
                .ok_or_else(|| anyhow::anyhow!("missing required argument `uris`"))?;
            let uris: Vec<String> = serde_json::from_value(uris_val.clone())
                .map_err(|e| anyhow::anyhow!("`uris` must be an array of strings: {e}"))?;
            ClientMessage::SimilarityMatrix { uris }
        }
        "lip_find_counterpart" => {
            let candidates_val = args
                .get("candidates")
                .ok_or_else(|| anyhow::anyhow!("missing required argument `candidates`"))?;
            let candidates: Vec<String> = serde_json::from_value(candidates_val.clone())
                .map_err(|e| anyhow::anyhow!("`candidates` must be an array of strings: {e}"))?;
            ClientMessage::FindSemanticCounterpart {
                uri: req_str(args, "uri")?,
                candidates,
                top_k: args["top_k"].as_u64().map(|n| n as usize).unwrap_or(5),
                filter: args["filter"].as_str().map(str::to_owned),
                min_score: args["min_score"].as_f64().map(|f| f as f32),
            }
        }
        "lip_coverage" => ClientMessage::QueryCoverage {
            root: req_str(args, "root")?,
        },
        "lip_find_boundaries" => ClientMessage::FindBoundaries {
            uri: req_str(args, "uri")?,
            chunk_lines: args["chunk_lines"]
                .as_u64()
                .map(|n| n as usize)
                .unwrap_or(30),
            threshold: args["threshold"].as_f64().map(|f| f as f32).unwrap_or(0.3),
            model: args["model"].as_str().map(str::to_owned),
        },
        "lip_semantic_diff" => ClientMessage::SemanticDiff {
            content_a: req_str(args, "content_a")?,
            content_b: req_str(args, "content_b")?,
            top_k: args["top_k"].as_u64().map(|n| n as usize).unwrap_or(5),
            model: args["model"].as_str().map(str::to_owned),
        },
        "lip_nearest_in_store" => {
            let store_val = args
                .get("store")
                .ok_or_else(|| anyhow::anyhow!("missing required argument `store`"))?;
            let store: std::collections::HashMap<String, Vec<f32>> =
                serde_json::from_value(store_val.clone())
                    .map_err(|e| anyhow::anyhow!("`store` must be a map of uri→[f32]: {e}"))?;
            ClientMessage::QueryNearestInStore {
                uri: req_str(args, "uri")?,
                store,
                top_k: args["top_k"].as_u64().map(|n| n as usize).unwrap_or(10),
                filter: args["filter"].as_str().map(str::to_owned),
                min_score: args["min_score"].as_f64().map(|f| f as f32),
            }
        }
        "lip_novelty_score" => {
            let uris_val = args
                .get("uris")
                .ok_or_else(|| anyhow::anyhow!("missing required argument `uris`"))?;
            let uris: Vec<String> = serde_json::from_value(uris_val.clone())
                .map_err(|e| anyhow::anyhow!("`uris` must be an array of strings: {e}"))?;
            ClientMessage::QueryNoveltyScore { uris }
        }
        "lip_extract_terminology" => {
            let uris_val = args
                .get("uris")
                .ok_or_else(|| anyhow::anyhow!("missing required argument `uris`"))?;
            let uris: Vec<String> = serde_json::from_value(uris_val.clone())
                .map_err(|e| anyhow::anyhow!("`uris` must be an array of strings: {e}"))?;
            ClientMessage::ExtractTerminology {
                uris,
                top_k: args["top_k"].as_u64().map(|n| n as usize).unwrap_or(20),
            }
        }
        "lip_prune_deleted" => ClientMessage::PruneDeleted,
        "lip_get_centroid" => {
            let uris_val = args
                .get("uris")
                .ok_or_else(|| anyhow::anyhow!("missing required argument `uris`"))?;
            let uris: Vec<String> = serde_json::from_value(uris_val.clone())
                .map_err(|e| anyhow::anyhow!("`uris` must be an array of strings: {e}"))?;
            ClientMessage::GetCentroid { uris }
        }
        "lip_stale_embeddings" => ClientMessage::QueryStaleEmbeddings {
            root: req_str(args, "root")?,
        },
        "lip_explain_match" => ClientMessage::ExplainMatch {
            query: req_str(args, "query")?,
            result_uri: req_str(args, "result_uri")?,
            top_k: args
                .get("top_k")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .unwrap_or(5),
            chunk_lines: args
                .get("chunk_lines")
                .and_then(|v| v.as_u64())
                .map(|v| v as usize)
                .unwrap_or(20),
            model: args
                .get("model")
                .and_then(|v| v.as_str())
                .map(str::to_owned),
        },
        other => anyhow::bail!("unknown LIP tool: {other}"),
    };

    query_daemon(socket, msg).await
}

// ── ServerMessage → human-readable text ──────────────────────────────────────

fn format_response(tool: &str, msg: &ServerMessage) -> String {
    match msg {
        ServerMessage::BlastRadiusResult(r) => {
            let mut out = format!(
                "Blast radius for `{}`:\n\
                 risk:                  {}{}\n\
                 direct dependents:     {}\n\
                 transitive dependents: {}",
                r.symbol_uri,
                r.risk_level,
                if r.truncated { " (truncated)" } else { "" },
                r.direct_dependents,
                r.transitive_dependents,
            );

            if !r.direct_items.is_empty() {
                out.push_str("\n\ndirect (distance 1):");
                for item in &r.direct_items {
                    let sym = if item.symbol_uri.is_empty() {
                        String::new()
                    } else {
                        format!(
                            "  #{}",
                            item.symbol_uri.split('#').next_back().unwrap_or("")
                        )
                    };
                    out.push_str(&format!("\n  {}{}", item.file_uri, sym));
                }
            }

            if !r.transitive_items.is_empty() {
                out.push_str("\n\ntransitive:");
                for item in &r.transitive_items {
                    out.push_str(&format!("\n  [d={}] {}", item.distance, item.file_uri));
                }
            }

            if r.direct_items.is_empty() && r.transitive_items.is_empty() {
                out.push_str("\n\naffected files: (none)");
            }

            out
        }
        ServerMessage::WorkspaceSymbolsResult { symbols } => {
            if symbols.is_empty() {
                return "No symbols found.".into();
            }
            symbols
                .iter()
                .map(|s| {
                    format!(
                        "{:<30} {:<12}  {}",
                        s.display_name,
                        format!("{:?}", s.kind),
                        s.uri
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        ServerMessage::DefinitionResult {
            symbol,
            location_uri,
            location_range,
        } => match (symbol, location_uri) {
            (Some(sym), Some(uri)) => {
                let pos = location_range
                    .as_ref()
                    .map(|r| format!("{}:{}", r.start_line + 1, r.start_char + 1))
                    .unwrap_or_default();
                let sig = sym.signature.as_deref().unwrap_or(&sym.display_name);
                format!("{uri}:{pos}\n```\n{sig}\n```")
            }
            _ => "Definition not found.".into(),
        },
        ServerMessage::ReferencesResult { occurrences } => {
            if occurrences.is_empty() {
                return "No references found.".into();
            }
            occurrences
                .iter()
                .map(|o| format!("{}  line {}", o.symbol_uri, o.range.start_line + 1))
                .collect::<Vec<_>>()
                .join("\n")
        }
        ServerMessage::HoverResult { symbol } => match symbol {
            Some(s) => {
                let sig = s.signature.as_deref().unwrap_or(&s.display_name);
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
        | ServerMessage::DeadSymbolsResult { symbols } => {
            if symbols.is_empty() {
                return if tool == "lip_dead_symbols" {
                    "No dead symbols found.".into()
                } else {
                    "No symbols in file.".into()
                };
            }
            symbols
                .iter()
                .map(|s| format!("{:<30} {:?}", s.display_name, s.kind))
                .collect::<Vec<_>>()
                .join("\n")
        }
        ServerMessage::AnnotationAck => "Annotation saved.".into(),
        ServerMessage::AnnotationValue { value } => {
            value.clone().unwrap_or_else(|| "(not set)".into())
        }
        // BatchQuery → per-slot ok/error results
        ServerMessage::BatchQueryResponse { results } => results
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let header = format!("[{i}]");
                match &r.ok {
                    Some(msg) => format!("{header}\n{}", format_response(tool, msg)),
                    None => format!(
                        "{header} error: {}",
                        r.error.as_deref().unwrap_or("unknown error")
                    ),
                }
            })
            .collect::<Vec<_>>()
            .join("\n---\n"),
        // Batch → one ServerMessage per request
        ServerMessage::BatchResult { results } => results
            .iter()
            .enumerate()
            .map(|(i, msg)| match msg {
                ServerMessage::Error { message, .. } => format!("[{i}] error: {message}"),
                other => format!("[{i}]\n{}", format_response(tool, other)),
            })
            .collect::<Vec<_>>()
            .join("\n---\n"),
        ServerMessage::SimilarSymbolsResult { symbols } => {
            if symbols.is_empty() {
                return "No similar symbols found.".into();
            }
            symbols
                .iter()
                .map(|s| {
                    format!(
                        "{:<30} {:<12}  score={:.2}  {}",
                        s.name, s.kind, s.score, s.uri,
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
        ServerMessage::StaleFilesResult { stale_uris } => {
            if stale_uris.is_empty() {
                return "All files are up to date.".into();
            }
            let mut out = format!(
                "{} stale file(s) — re-send Delta::Upsert for each:\n",
                stale_uris.len()
            );
            for uri in stale_uris {
                out.push_str(&format!("  {uri}\n"));
            }
            out
        }
        ServerMessage::SymbolUpgraded {
            uri,
            old_confidence,
            new_confidence,
        } => {
            format!("upgraded {uri}: confidence {old_confidence} → {new_confidence}")
        }
        ServerMessage::EmbeddingBatchResult {
            vectors,
            model,
            dims,
        } => {
            let computed = vectors.iter().filter(|v| v.is_some()).count();
            format!(
                "embedded {computed}/{total} files  model={model}  dims={dims}",
                total = vectors.len()
            )
        }
        ServerMessage::IndexStatusResult {
            indexed_files,
            pending_embedding_files,
            last_updated_ms,
            embedding_model,
            mixed_models,
            models_in_index,
            tier3_sources,
        } => {
            let last = last_updated_ms
                .map(|ms| format!("  last_updated={ms}ms"))
                .unwrap_or_default();
            let model = embedding_model
                .as_deref()
                .map(|m| format!("  embedding_model={m}"))
                .unwrap_or_else(|| "  embedding_model=(not configured)".into());
            let mixed = if *mixed_models {
                format!("  ⚠ MIXED MODELS ({})", models_in_index.join(", "))
            } else {
                String::new()
            };
            let tier3 = if tier3_sources.is_empty() {
                String::new()
            } else {
                let parts: Vec<String> = tier3_sources
                    .iter()
                    .map(|s| {
                        format!(
                            "{}@{}/{} imported_at={}ms",
                            s.tool_name, s.tool_version, s.source_id, s.imported_at_ms
                        )
                    })
                    .collect();
                format!("  tier3=[{}]", parts.join(", "))
            };
            format!("indexed={indexed_files}  pending_embeddings={pending_embedding_files}{last}{model}{mixed}{tier3}")
        }
        ServerMessage::FileStatusResult {
            uri,
            indexed,
            has_embedding,
            age_seconds,
            embedding_model,
        } => {
            let age = age_seconds
                .map(|s| format!("  age={s}s"))
                .unwrap_or_default();
            let model = embedding_model
                .as_deref()
                .map(|m| format!("  embedding_model={m}"))
                .unwrap_or_default();
            format!("{uri}  indexed={indexed}  has_embedding={has_embedding}{age}{model}")
        }
        ServerMessage::NearestResult { results } => {
            if results.is_empty() {
                return "No nearest neighbours found.".into();
            }
            results
                .iter()
                .map(|r| format!("score={:.4}  {}", r.score, r.uri))
                .collect::<Vec<_>>()
                .join("\n")
        }
        ServerMessage::BoundariesResult { uri, boundaries } => {
            if boundaries.is_empty() {
                return format!("{uri}: no semantic boundaries detected above threshold");
            }
            let mut out = format!("{uri}  ({} boundaries)\n", boundaries.len());
            for b in boundaries {
                out.push_str(&format!(
                    "  lines {:>5}–{:<5}  shift={:.4}\n",
                    b.start_line, b.end_line, b.shift_magnitude
                ));
            }
            out.trim_end().to_owned()
        }
        ServerMessage::SemanticDiffResult {
            distance,
            moving_toward,
        } => {
            let mut out = format!("drift={distance:.4}");
            if !moving_toward.is_empty() {
                out.push_str("  moving toward:");
                for r in moving_toward {
                    out.push_str(&format!("\n  score={:.4}  {}", r.score, r.uri));
                }
            }
            out
        }
        ServerMessage::NoveltyScoreResult { score, per_file } => {
            let mut out = format!("novelty score={score:.4}  ({} files)\n", per_file.len());
            for item in per_file {
                let nearest = item
                    .nearest_existing
                    .as_deref()
                    .unwrap_or("(none — no other embeddings)");
                out.push_str(&format!(
                    "  novelty={:.4}  {}  nearest={}\n",
                    item.score, item.uri, nearest
                ));
            }
            out.trim_end().to_owned()
        }
        ServerMessage::TerminologyResult { terms } => {
            if terms.is_empty() {
                return "No terminology extracted (ensure symbol embeddings are populated \
                         via lip_embedding_batch with lip:// URIs)."
                    .into();
            }
            terms
                .iter()
                .map(|t| format!("score={:.4}  {:<30}  {}", t.score, t.term, t.source_uri))
                .collect::<Vec<_>>()
                .join("\n")
        }
        ServerMessage::PruneDeletedResult { checked, removed } => {
            if removed.is_empty() {
                format!("checked={checked}  removed=0  index is clean")
            } else {
                let mut out = format!("checked={checked}  removed={}:\n", removed.len());
                for uri in removed {
                    out.push_str(&format!("  {uri}\n"));
                }
                out.trim_end().to_owned()
            }
        }
        ServerMessage::OutliersResult { outliers } => {
            if outliers.is_empty() {
                return "No outliers found (no embeddings for the given URIs).".into();
            }
            outliers
                .iter()
                .map(|r| format!("mean_sim={:.4}  {}", r.score, r.uri))
                .collect::<Vec<_>>()
                .join("\n")
        }
        ServerMessage::SemanticDriftResult { distance } => match distance {
            Some(d) => format!("drift={d:.4}  (cosine distance; 0.0=identical, 2.0=opposite)"),
            None => "no cached embeddings for one or both URIs — call embedding_batch first".into(),
        },
        ServerMessage::SimilarityMatrixResult { uris, matrix } => {
            if uris.is_empty() {
                return "No indexed URIs with embeddings in the provided list.".into();
            }
            // Header row with short labels
            let labels: Vec<String> = uris
                .iter()
                .map(|u| {
                    u.rsplit('/')
                        .next()
                        .unwrap_or(u.as_str())
                        .chars()
                        .take(12)
                        .collect()
                })
                .collect();
            let col_w = 7usize;
            let row_label_w = labels.iter().map(|l| l.len()).max().unwrap_or(0).max(4);
            let mut out = format!("{:<row_label_w$}", "");
            for label in &labels {
                out.push_str(&format!("  {:>col_w$}", label));
            }
            for (i, row) in matrix.iter().enumerate() {
                out.push_str(&format!("\n{:<row_label_w$}", labels[i]));
                for val in row {
                    out.push_str(&format!("  {:>col_w$.4}", val));
                }
            }
            out
        }
        ServerMessage::CoverageResult {
            root,
            total_files,
            embedded_files,
            coverage_fraction,
            by_directory,
        } => {
            let pct = coverage_fraction
                .map(|f| format!("{:.1}%", f * 100.0))
                .unwrap_or_else(|| "n/a".into());
            let mut out = format!(
                "coverage: {embedded_files}/{total_files} files embedded ({pct})  root={root}"
            );
            if !by_directory.is_empty() {
                out.push_str("\n\nby directory:");
                for dir in by_directory {
                    let dir_pct = if dir.total_files > 0 {
                        format!(
                            "{:.0}%",
                            dir.embedded_files as f32 / dir.total_files as f32 * 100.0
                        )
                    } else {
                        "n/a".into()
                    };
                    out.push_str(&format!(
                        "\n  {:<5}  {}/{} embedded  {}",
                        dir_pct, dir.embedded_files, dir.total_files, dir.directory
                    ));
                }
            }
            out
        }
        ServerMessage::CentroidResult { vector, included } => {
            if vector.is_empty() {
                "No embeddings found for the given URIs — call lip_embedding_batch first.".into()
            } else {
                format!(
                    "centroid computed from {included} file(s)  dim={}  \
                     first_3=[{:.4}, {:.4}, {:.4}]",
                    vector.len(),
                    vector.first().copied().unwrap_or(0.0),
                    vector.get(1).copied().unwrap_or(0.0),
                    vector.get(2).copied().unwrap_or(0.0),
                )
            }
        }
        ServerMessage::StaleEmbeddingsResult { uris } => {
            if uris.is_empty() {
                "All embeddings under the given root are fresh.".into()
            } else {
                let mut out = format!("{} file(s) have stale embeddings:\n", uris.len());
                for uri in uris {
                    out.push_str(&format!("  {uri}\n"));
                }
                out.trim_end().to_owned()
            }
        }
        ServerMessage::ExplainMatchResult {
            chunks,
            query_model,
        } => {
            if chunks.is_empty() {
                return "No explanation chunks found.".into();
            }
            let mut out = format!("Top {} chunk(s)  model={query_model}\n", chunks.len());
            for (i, c) in chunks.iter().enumerate() {
                out.push_str(&format!(
                    "\n[{}] lines {}-{}  score={:.4}\n{}\n",
                    i + 1,
                    c.start_line,
                    c.end_line,
                    c.score,
                    c.chunk_text
                ));
            }
            out.trim_end().to_owned()
        }
        ServerMessage::Error { message, .. } => format!("LIP error: {message}"),
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

// ── Argument helpers ──────────────────────────────────────────────────────────

fn req_str(args: &Value, key: &str) -> anyhow::Result<String> {
    args[key]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("missing required argument `{key}`"))
}

fn req_u32(args: &Value, key: &str) -> anyhow::Result<u32> {
    args[key]
        .as_u64()
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
                        "description": "e.g. 'team:owner', 'lip:fragile', 'agent:note', 'lip:nyx-agent-lock'"
                    },
                    "value":     { "type": "string" },
                    "author_id": {
                        "type": "string",
                        "description": "e.g. 'agent:claude' or 'human:alice'"
                    }
                },
                "required": ["symbol_uri", "key", "value", "author_id"]
            }
        },
        {
            "name": "lip_similar_symbols",
            "description": "Trigram fuzzy-search across all tracked symbol names and documentation. \
                            Useful when you know roughly what a symbol is called but not its exact name \
                            or location. Returns URI, kind, and relevance score.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Partial name or keyword to search for" },
                    "limit": { "type": "integer", "default": 20 }
                },
                "required": ["query"]
            }
        },
        {
            "name": "lip_annotation_workspace_list",
            "description": "Search annotations across ALL symbols by key prefix. \
                            Use to find all lip:fragile symbols, all agent:note entries, \
                            or every annotation with a given prefix workspace-wide. \
                            Pass an empty string to list every annotation.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "key_prefix": {
                        "type": "string",
                        "description": "Key prefix to filter by, e.g. 'lip:fragile', 'agent:', 'team:'. \
                                        Empty string returns all annotations."
                    }
                },
                "required": []
            }
        },
        {
            "name": "lip_stale_files",
            "description": "Merkle sync probe: given the client's per-file content hashes, \
                            returns URIs that are stale (daemon hash differs) or unknown. \
                            One round-trip on reconnect — the client then re-sends Delta::Upsert \
                            only for the returned URIs rather than re-indexing everything.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "files": {
                        "type": "array",
                        "description": "Array of [uri, sha256_hex] pairs",
                        "items": {
                            "type": "array",
                            "prefixItems": [
                                { "type": "string", "description": "File URI (file:///…)" },
                                { "type": "string", "description": "SHA-256 hex of the file content" }
                            ],
                            "minItems": 2,
                            "maxItems": 2
                        }
                    }
                },
                "required": ["files"]
            }
        },
        {
            "name": "lip_load_slice",
            "description": "Mount a pre-built dependency slice into the daemon's symbol graph. \
                            All symbols are loaded at Tier 3 confidence (score=100). \
                            Idempotent — re-loading the same package replaces prior symbols. \
                            Pass the OwnedDependencySlice JSON object returned by lip_fetch or \
                            fetched from the registry.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "slice": {
                        "type": "object",
                        "description": "OwnedDependencySlice JSON (from lip fetch or registry)"
                    }
                },
                "required": ["slice"]
            }
        },
        {
            "name": "lip_embedding_batch",
            "description": "Compute and cache embedding vectors for a list of file URIs. \
                            Uses the endpoint configured via LIP_EMBEDDING_URL (OpenAI-compatible). \
                            Already-cached embeddings are returned without a network call. \
                            Call this before lip_nearest to ensure vectors are populated.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uris": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "File URIs to embed (file:///…)"
                    },
                    "model": {
                        "type": "string",
                        "description": "Override the embedding model for this request"
                    }
                },
                "required": ["uris"]
            }
        },
        {
            "name": "lip_index_status",
            "description": "Report overall daemon health: number of indexed files, \
                            pending embedding count, timestamp of last update, and configured model. \
                            Use as a quick ckb-doctor check.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        },
        {
            "name": "lip_file_status",
            "description": "Report the indexing status of a single file: \
                            whether it is indexed, whether it has an embedding, and its age.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uri": { "type": "string", "description": "File URI (file:///…)" }
                },
                "required": ["uri"]
            }
        },
        {
            "name": "lip_nearest",
            "description": "Find the top-K files most semantically similar to a given file, \
                            using pre-computed embedding vectors (cosine similarity). \
                            The file must have an embedding — call lip_embedding_batch first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uri":       { "type": "string",  "description": "Query file URI" },
                    "top_k":     { "type": "integer", "default": 10 },
                    "filter":    { "type": "string",  "description": "Glob to restrict candidates \
                                   (e.g. 'internal/auth/**' or '*_test.go'). \
                                   Patterns with '/' match the full path; others match the filename." },
                    "min_score": { "type": "number",  "description": "Minimum cosine similarity \
                                   threshold [0.0, 1.0]. Results below this score are dropped." }
                },
                "required": ["uri"]
            }
        },
        {
            "name": "lip_nearest_by_text",
            "description": "Find the top-K files most semantically similar to a free-text query. \
                            The daemon embeds the text on the fly and runs cosine search. \
                            Useful for 'find files related to authentication' style queries.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "text":      { "type": "string",  "description": "Natural language query" },
                    "top_k":     { "type": "integer", "default": 10 },
                    "model":     { "type": "string",  "description": "Override embedding model" },
                    "filter":    { "type": "string",  "description": "See lip_nearest.filter" },
                    "min_score": { "type": "number",  "description": "See lip_nearest.min_score" }
                },
                "required": ["text"]
            }
        },
        {
            "name": "lip_nearest_by_contrast",
            "description": "Contrastive semantic search: find files similar to `like_uri` \
                            but different from `unlike_uri`. \
                            Computes normalize(embed(like) − embed(unlike)) then runs cosine search. \
                            Example: like=new_auth.rs unlike=legacy_auth.rs → files in the style \
                            of the new module but not the old one. \
                            Both URIs must have cached embeddings — call lip_embedding_batch first.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "like_uri":   { "type": "string",  "description": "URI to move towards" },
                    "unlike_uri": { "type": "string",  "description": "URI to move away from" },
                    "top_k":      { "type": "integer", "default": 10 },
                    "filter":     { "type": "string",  "description": "See lip_nearest.filter" },
                    "min_score":  { "type": "number",  "description": "See lip_nearest.min_score" }
                },
                "required": ["like_uri", "unlike_uri"]
            }
        },
        {
            "name": "lip_outliers",
            "description": "Identify semantically misplaced files within a set. \
                            For each URI computes its leave-one-out mean cosine similarity \
                            to the rest of the group; returns the top_k lowest-scoring files. \
                            Useful for finding files that conceptually don't belong in a package \
                            even when they are structurally co-located.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uris":  {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "File URIs to analyse (all must have cached embeddings)"
                    },
                    "top_k": { "type": "integer", "default": 5 }
                },
                "required": ["uris"]
            }
        },
        {
            "name": "lip_semantic_drift",
            "description": "Measure how semantically different two files are. \
                            Returns cosine distance in [0.0, 2.0]: 0.0 = identical meaning, \
                            ~0.3 = similar, ~1.0 = unrelated, 2.0 = opposite. \
                            Useful for tracking how much a module's identity has shifted \
                            between versions. Both URIs must have cached embeddings.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uri_a": { "type": "string", "description": "First file URI" },
                    "uri_b": { "type": "string", "description": "Second file URI" }
                },
                "required": ["uri_a", "uri_b"]
            }
        },
        {
            "name": "lip_similarity_matrix",
            "description": "Compute all pairwise cosine similarities for a list of files \
                            in a single call. Returns a labelled N×N matrix. \
                            Useful for building a semantic coupling graph over a module — \
                            two files can be tightly coupled conceptually even if they \
                            never co-change. URIs without embeddings are silently excluded.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uris": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "File URIs to compare"
                    }
                },
                "required": ["uris"]
            }
        },
        {
            "name": "lip_find_counterpart",
            "description": "Given a source file and a pool of candidates, return the candidates \
                            most semantically similar to the source. \
                            Finds test files that cover a changed implementation even when naming \
                            conventions differ or tests live in a separate repo. \
                            The source URI must have a cached embedding.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uri": {
                        "type": "string",
                        "description": "The implementation file to match against"
                    },
                    "candidates": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Pool of candidate URIs to rank (e.g. all test files)"
                    },
                    "top_k":     { "type": "integer", "default": 5 },
                    "filter":    { "type": "string",  "description": "See lip_nearest.filter" },
                    "min_score": { "type": "number",  "description": "See lip_nearest.min_score" }
                },
                "required": ["uri", "candidates"]
            }
        },
        {
            "name": "lip_coverage",
            "description": "Report embedding coverage under a filesystem path. \
                            Shows what percentage of indexed files have embeddings, \
                            broken down by directory. \
                            Use to diagnose silent degradation during warm-up: \
                            semantic search quality is proportional to coverage.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "root": {
                        "type": "string",
                        "description": "Filesystem path prefix to scope the report, \
                                        e.g. \"/project/src\""
                    }
                },
                "required": ["root"]
            }
        },
        {
            "name": "lip_find_boundaries",
            "description": "Detect semantic boundaries within a file by chunking it into \
                            line-windows and embedding each window. Returns the positions \
                            where meaning shifts significantly — useful for identifying \
                            natural split points during extract refactors, or for \
                            understanding how a file is conceptually organized beyond its \
                            AST structure. Requires LIP_EMBEDDING_URL.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uri":         { "type": "string",  "description": "File URI to scan" },
                    "chunk_lines": { "type": "integer", "default": 30,
                                     "description": "Lines per embedding window" },
                    "threshold":   { "type": "number",  "default": 0.3,
                                     "description": "Min cosine distance to report (0.0–2.0)" },
                    "model":       { "type": "string",  "description": "Override embedding model" }
                },
                "required": ["uri"]
            }
        },
        {
            "name": "lip_semantic_diff",
            "description": "Measure how much the semantic content of a file changed between \
                            two versions. Returns a drift distance (0.0 = identical, 2.0 = opposite) \
                            and the nearest files to the *direction* of change — naming the concepts \
                            the content moved toward. Catches semantic breaking changes that \
                            structural diffs miss: a renamed function whose body quietly changed \
                            to do something different. Requires LIP_EMBEDDING_URL.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content_a": { "type": "string", "description": "Old file content" },
                    "content_b": { "type": "string", "description": "New file content" },
                    "top_k":     { "type": "integer", "default": 5 },
                    "model":     { "type": "string",  "description": "Override embedding model" }
                },
                "required": ["content_a", "content_b"]
            }
        },
        {
            "name": "lip_nearest_in_store",
            "description": "Semantic nearest-neighbour search against a caller-provided \
                            embedding store. Use for cross-repo federation: export embeddings \
                            from each repo root via lip_embedding_batch with ExportEmbeddings, \
                            merge the maps, then search across all repos in one call. \
                            The query URI must have a cached embedding in the local daemon.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uri":       { "type": "string",  "description": "Query file URI (must be embedded locally)" },
                    "store":     { "type": "object",  "description": "External embedding store: map of uri→[f32]" },
                    "top_k":     { "type": "integer", "default": 10 },
                    "filter":    { "type": "string",  "description": "See lip_nearest.filter" },
                    "min_score": { "type": "number",  "description": "See lip_nearest.min_score" }
                },
                "required": ["uri", "store"]
            }
        },
        {
            "name": "lip_novelty_score",
            "description": "Quantify how semantically novel a set of files is relative to \
                            the rest of the codebase. For each file finds its nearest existing \
                            neighbour (outside the set) and returns 1 − similarity as novelty. \
                            High novelty means the PR introduces concepts not seen elsewhere — \
                            worth extra review attention regardless of structural complexity.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uris": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "File URIs to score (typically the PR diff set)"
                    }
                },
                "required": ["uris"]
            }
        },
        {
            "name": "lip_extract_terminology",
            "description": "Extract the domain vocabulary most semantically central to a \
                            set of files. Ranks symbol display names by their proximity to \
                            the centroid of the input files' embeddings. Surfaces the implicit \
                            vocabulary — terms that are conceptually load-bearing even when \
                            they don't appear as prominent symbol names. \
                            Requires symbol embeddings (call lip_embedding_batch with lip:// URIs).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uris": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "File URIs whose symbols to rank"
                    },
                    "top_k": { "type": "integer", "default": 20 }
                },
                "required": ["uris"]
            }
        },
        {
            "name": "lip_prune_deleted",
            "description": "Remove index entries for files that no longer exist on disk. \
                            On repos with high churn, ghost embeddings accumulate and pollute \
                            nearest-neighbour results. Run periodically or before any semantic \
                            search on a stale index. Returns a count of checked and removed files.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        },
        {
            "name": "lip_get_centroid",
            "description": "Compute and return the embedding centroid (component-wise mean) of \
                            a set of files without shipping all raw vectors over the socket. \
                            Use in getArchitecture to characterise a module's semantic meaning, \
                            for federation (compare module centroids across repos), or as the \
                            query vector for lip_nearest_in_store. Returns the centroid vector \
                            and the count of URIs that contributed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "uris": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "File (or symbol) URIs to average."
                    }
                },
                "required": ["uris"]
            }
        },
        {
            "name": "lip_stale_embeddings",
            "description": "Report files under `root` whose stored embedding is older than \
                            their current filesystem mtime. Detects the case where LIP was \
                            offline during a batch of writes and search results may be stale. \
                            lip_file_status answers 'is this file indexed'; this answers \
                            'is the semantic index actually fresh'. Returns a list of URIs.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "root": {
                        "type": "string",
                        "description": "Filesystem path prefix to scope the scan \
                                       (e.g. \"/project/src\")."
                    }
                },
                "required": ["root"]
            }
        },
        {
            "name": "lip_explain_match",
            "description": "Explain WHY result_uri was a strong semantic match for a query. \
                            Chunks result_uri's source into windows, embeds each chunk, and \
                            scores against the query embedding. Returns the top-k chunks with \
                            line ranges and contribution scores. Use when you need to tell the \
                            user which part of a file is semantically relevant, not just that \
                            the file is relevant.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "A file URI (uses its cached embedding) or free-text \
                                       query (embedded on the fly)."
                    },
                    "result_uri": {
                        "type": "string",
                        "description": "URI of the file whose source will be chunked and scored."
                    },
                    "top_k": {
                        "type": "integer",
                        "description": "Number of top-scoring chunks to return (default 5)."
                    },
                    "chunk_lines": {
                        "type": "integer",
                        "description": "Lines per chunk window (default 20)."
                    },
                    "model": {
                        "type": "string",
                        "description": "Override embedding model for this request."
                    }
                },
                "required": ["query", "result_uri"]
            }
        },
        {
            "name": "lip_batch_query",
            "description": "Execute multiple queries in a single round-trip — \
                            one socket connection instead of N. \
                            Use for planning: blast_radius + references + annotation_get \
                            for 10 symbols costs 1 round-trip, not 30. \
                            Each query object must carry a `type` field \
                            (e.g. 'query_blast_radius', 'query_references', 'annotation_get'). \
                            Manifest and Delta are not permitted inside a batch.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "queries": {
                        "type": "array",
                        "description": "Array of query objects, each with a `type` field",
                        "items": {
                            "type": "object",
                            "properties": {
                                "type": {
                                    "type": "string",
                                    "enum": [
                                        "query_blast_radius",
                                        "query_references",
                                        "query_definition",
                                        "query_hover",
                                        "query_workspace_symbols",
                                        "query_document_symbols",
                                        "query_dead_symbols",
                                        "annotation_get",
                                        "annotation_set",
                                        "annotation_list",
                                        "similarity",
                                        "export_embeddings",
                                        "query_nearest_by_contrast",
                                        "query_outliers",
                                        "query_semantic_drift",
                                        "similarity_matrix",
                                        "find_semantic_counterpart",
                                        "query_coverage",
                                        "query_nearest_in_store",
                                        "query_novelty_score",
                                        "extract_terminology",
                                        "get_centroid"
                                    ]
                                }
                            },
                            "required": ["type"]
                        }
                    }
                },
                "required": ["queries"]
            }
        }
    ])
}
