/// End-to-end integration tests: daemon ↔ client over a real Unix socket.
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use lip_core::daemon::LipDaemon;
use lip_core::query_graph::{ClientMessage, ErrorCode, ServerMessage};
use lip_core::schema::{
    Action, ExtractionTier, IndexingState, ModifiersSource, OwnedDocument, OwnedOccurrence,
    OwnedRange, OwnedSymbolInfo, ReferenceKind, Role, SymbolKind, Visibility,
};

// ─── Framing helpers (client side) ───────────────────────────────────────────

async fn send(stream: &mut UnixStream, msg: &ClientMessage) -> anyhow::Result<()> {
    let body = serde_json::to_vec(msg)?;
    stream.write_all(&(body.len() as u32).to_be_bytes()).await?;
    stream.write_all(&body).await?;
    Ok(())
}

async fn recv_raw(stream: &mut UnixStream) -> anyhow::Result<ServerMessage> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

/// Read the next non-notification message, discarding any push events
/// (`IndexChanged`, `SymbolUpgraded`) that the daemon may have sent between
/// responses. Tests that expect a specific query response use this.
async fn recv(stream: &mut UnixStream) -> anyhow::Result<ServerMessage> {
    loop {
        let msg = recv_raw(stream).await?;
        match msg {
            ServerMessage::IndexChanged { .. } | ServerMessage::SymbolUpgraded { .. } => continue,
            other => return Ok(other),
        }
    }
}

// ─── Helper: build a document with known source ───────────────────────────────

fn make_doc(uri: &str, source: &str) -> OwnedDocument {
    OwnedDocument {
        uri: uri.to_owned(),
        content_hash: lip_core::schema::sha256_hex(source.as_bytes()),
        language: "rust".to_owned(),
        occurrences: vec![],
        symbols: vec![],
        merkle_path: uri.to_owned(),
        edges: vec![],
        source_text: Some(source.to_owned()),
    }
}

// ─── Full pipeline ────────────────────────────────────────────────────────────

#[tokio::test]
async fn daemon_full_pipeline() {
    // Use a temp file path as the socket.
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("lip_test.sock");

    // Spawn the daemon as a background task.
    let daemon = LipDaemon::new(&socket_path);
    let daemon_task = tokio::spawn(async move {
        // run() loops forever; we abort the task after the test.
        let _ = daemon.run().await;
    });

    // Wait briefly for the daemon to bind the socket.
    tokio::time::sleep(Duration::from_millis(20)).await;

    // Connect.
    let mut client = UnixStream::connect(&socket_path)
        .await
        .expect("connect to daemon");

    // ── 1. Handshake ──────────────────────────────────────────────────────────
    send(
        &mut client,
        &ClientMessage::Manifest(lip_core::daemon::ManifestRequest {
            repo_root: "/tmp/test-repo".to_owned(),
            merkle_root: "abc123".to_owned(),
            dep_tree_hash: "def456".to_owned(),
            lip_version: "0.1.0".to_owned(),
        }),
    )
    .await
    .expect("send manifest");

    let resp = recv(&mut client).await.expect("recv manifest response");
    match resp {
        ServerMessage::ManifestResponse(r) => {
            // Daemon echoes back the merkle_root. Fresh daemon = Cold state.
            assert_eq!(r.cached_merkle_root, "abc123");
            assert!(r.missing_slices.is_empty());
            // First connect with no prior state → Cold.
            assert_eq!(r.indexing_state, IndexingState::Cold);
        }
        other => panic!("expected ManifestResponse, got {other:?}"),
    }

    // ── 2. Upsert a file ──────────────────────────────────────────────────────
    let source = r#"
pub struct Greeter;
impl Greeter {
    pub fn hello(&self) -> &str { "hello" }
}
"#;
    let uri = "lip://local/test@0.1/greeter.rs";

    send(
        &mut client,
        &ClientMessage::Delta {
            seq: 42,
            action: Action::Upsert,
            document: make_doc(uri, source),
        },
    )
    .await
    .expect("send upsert delta");

    let resp = recv(&mut client).await.expect("recv delta ack");
    match resp {
        ServerMessage::DeltaAck { seq, accepted, .. } => {
            assert_eq!(seq, 42);
            assert!(accepted);
        }
        other => panic!("expected DeltaAck, got {other:?}"),
    }

    // ── 3. Query definition ───────────────────────────────────────────────────
    send(
        &mut client,
        &ClientMessage::QueryDefinition {
            uri: uri.to_owned(),
            line: 0,
            col: 0,
        },
    )
    .await
    .expect("send query");

    let resp = recv(&mut client).await.expect("recv definition");
    // The Tier 1 indexer should have extracted at least one symbol from the
    // file. We just assert we get a DefinitionResult back (symbol may be
    // Some or None depending on tree-sitter grammar support in CI).
    assert!(
        matches!(resp, ServerMessage::DefinitionResult { .. }),
        "expected DefinitionResult, got {resp:?}"
    );

    // ── 4. Delete the file ────────────────────────────────────────────────────
    send(
        &mut client,
        &ClientMessage::Delta {
            seq: 43,
            action: Action::Delete,
            document: make_doc(uri, ""),
        },
    )
    .await
    .expect("send delete delta");

    let resp = recv(&mut client).await.expect("recv delete ack");
    match resp {
        ServerMessage::DeltaAck { seq, accepted, .. } => {
            assert_eq!(seq, 43);
            assert!(accepted);
        }
        other => panic!("expected DeltaAck after delete, got {other:?}"),
    }

    // ── Cleanup ───────────────────────────────────────────────────────────────
    daemon_task.abort();
    let _ = daemon_task.await; // JoinError::Cancelled is expected
}

// ─── Workspace-symbols integration ───────────────────────────────────────────

#[tokio::test]
async fn daemon_workspace_symbols() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("lip_ws.sock");

    let daemon = LipDaemon::new(&socket_path);
    let daemon_task = tokio::spawn(async move { daemon.run().await.ok() });

    tokio::time::sleep(Duration::from_millis(20)).await;
    let mut client = UnixStream::connect(&socket_path).await.expect("connect");

    // Index two files.
    for i in 0..2 {
        let source =
            format!("pub struct Widget{i}; pub fn make_{i}() -> Widget{i} {{ Widget{i} }}");
        let uri = format!("lip://local/test@0.1/w{i}.rs");
        send(
            &mut client,
            &ClientMessage::Delta {
                seq: i as u64,
                action: Action::Upsert,
                document: make_doc(&uri, &source),
            },
        )
        .await
        .expect("send");
        let _ = recv(&mut client).await.expect("recv");
    }

    // Query workspace symbols matching "Widget".
    send(
        &mut client,
        &ClientMessage::QueryWorkspaceSymbols {
            kind_filter: None,
            scope: None,
            modifier_filter: None,
            query: "Widget".to_owned(),
            limit: Some(50),
        },
    )
    .await
    .expect("send workspace query");

    let resp = recv(&mut client).await.expect("recv workspace symbols");
    match resp {
        ServerMessage::WorkspaceSymbolsResult { symbols, .. } => {
            // tree-sitter should have found at least the two struct declarations.
            assert!(
                !symbols.is_empty(),
                "expected at least one Widget symbol, got none"
            );
        }
        other => panic!("expected WorkspaceSymbolsResult, got {other:?}"),
    }

    daemon_task.abort();
    let _ = daemon_task.await;
}

// ─── Journal persistence across restart ──────────────────────────────────────

/// Index a file, kill the daemon, restart it on the same socket path (same
/// journal), and verify the symbol is still queryable without re-sending a
/// Delta. This is the primary correctness test for the write-ahead journal.
#[tokio::test]
async fn daemon_restart_restores_journal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("lip_journal.sock");

    let source = "pub fn persisted_fn() {}";
    let uri = "lip://local/test@0.1/persist.rs";

    // ── First daemon run ─────────────────────────────────────────────────────
    {
        let daemon = LipDaemon::new(&socket_path);
        let task = tokio::spawn(async move { daemon.run().await.ok() });
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut client = UnixStream::connect(&socket_path).await.unwrap();

        // Upsert a file so the journal gets a UpsertFile entry.
        send(
            &mut client,
            &ClientMessage::Delta {
                seq: 1,
                action: Action::Upsert,
                document: make_doc(uri, source),
            },
        )
        .await
        .unwrap();
        let _ = recv(&mut client).await.unwrap(); // DeltaAck

        // Also set a merkle root so we can verify lifecycle state on restart.
        send(
            &mut client,
            &ClientMessage::Manifest(lip_core::daemon::ManifestRequest {
                repo_root: "/tmp/persist-repo".into(),
                merkle_root: "persist-hash".into(),
                dep_tree_hash: String::new(),
                lip_version: "0.1.0".into(),
            }),
        )
        .await
        .unwrap();
        let _ = recv(&mut client).await.unwrap(); // ManifestResponse

        task.abort();
        let _ = task.await;
        // Socket file is removed by the daemon on next bind; journal stays.
    }

    // Brief pause to let the OS release the socket fd.
    tokio::time::sleep(Duration::from_millis(10)).await;

    // ── Second daemon run — same directory, same journal ─────────────────────
    {
        let daemon = LipDaemon::new(&socket_path);
        let task = tokio::spawn(async move { daemon.run().await.ok() });
        tokio::time::sleep(Duration::from_millis(30)).await;

        let mut client = UnixStream::connect(&socket_path).await.unwrap();

        // Query workspace symbols — no Delta sent this run.
        send(
            &mut client,
            &ClientMessage::QueryWorkspaceSymbols {
            kind_filter: None,
            scope: None,
            modifier_filter: None,
                query: "persisted".into(),
                limit: Some(10),
            },
        )
        .await
        .unwrap();

        let resp = recv(&mut client).await.unwrap();
        match resp {
            ServerMessage::WorkspaceSymbolsResult { symbols, .. } => {
                assert!(
                    !symbols.is_empty(),
                    "expected persisted_fn to survive daemon restart, got no symbols"
                );
                assert!(
                    symbols.iter().any(|s| s.display_name.contains("persisted")),
                    "expected a symbol named 'persisted*', got: {symbols:?}"
                );
            }
            other => panic!("expected WorkspaceSymbolsResult, got {other:?}"),
        }

        // Manifest with the same merkle root should report WarmFull — the db
        // was fully restored from the journal.
        send(
            &mut client,
            &ClientMessage::Manifest(lip_core::daemon::ManifestRequest {
                repo_root: "/tmp/persist-repo".into(),
                merkle_root: "persist-hash".into(),
                dep_tree_hash: String::new(),
                lip_version: "0.1.0".into(),
            }),
        )
        .await
        .unwrap();

        let resp = recv(&mut client).await.unwrap();
        match resp {
            ServerMessage::ManifestResponse(r) => {
                assert_eq!(
                    r.indexing_state,
                    IndexingState::WarmFull,
                    "expected WarmFull after journal replay, got {:?}",
                    r.indexing_state
                );
            }
            other => panic!("expected ManifestResponse, got {other:?}"),
        }

        task.abort();
        let _ = task.await;
    }
}

// ─── QueryDeadSymbols ─────────────────────────────────────────────────────────

#[tokio::test]
async fn daemon_query_dead_symbols() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("lip_dead.sock");

    let daemon = LipDaemon::new(&socket_path);
    let task = tokio::spawn(async move { daemon.run().await.ok() });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let mut client = UnixStream::connect(&socket_path).await.unwrap();

    send(
        &mut client,
        &ClientMessage::Delta {
            seq: 1,
            action: Action::Upsert,
            document: make_doc(
                "lip://local/test@0.1/dead.rs",
                "pub fn orphan_a() {} pub fn orphan_b() {}",
            ),
        },
    )
    .await
    .unwrap();
    let _ = recv(&mut client).await.unwrap();

    send(
        &mut client,
        &ClientMessage::QueryDeadSymbols { limit: Some(50) },
    )
    .await
    .unwrap();

    let resp = recv(&mut client).await.unwrap();
    match resp {
        ServerMessage::DeadSymbolsResult { symbols } => {
            assert!(!symbols.is_empty(), "expected dead symbols, got none");
        }
        other => panic!("expected DeadSymbolsResult, got {other:?}"),
    }

    task.abort();
    let _ = task.await;
}

// ─── QueryReferences ─────────────────────────────────────────────────────────

#[tokio::test]
async fn daemon_query_references() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("lip_refs.sock");

    let daemon = LipDaemon::new(&socket_path);
    let task = tokio::spawn(async move { daemon.run().await.ok() });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let mut client = UnixStream::connect(&socket_path).await.unwrap();

    let uri = "lip://local/test@0.1/refs.rs";
    send(
        &mut client,
        &ClientMessage::Delta {
            seq: 1,
            action: Action::Upsert,
            document: make_doc(
                uri,
                "pub fn referenced() {} pub fn caller() { referenced(); }",
            ),
        },
    )
    .await
    .unwrap();
    let _ = recv(&mut client).await.unwrap();

    send(
        &mut client,
        &ClientMessage::QueryDocumentSymbols {
            uri: uri.to_owned(),
        },
    )
    .await
    .unwrap();
    let syms_resp = recv(&mut client).await.unwrap();
    let sym_uri = match syms_resp {
        ServerMessage::DocumentSymbolsResult { symbols } if !symbols.is_empty() => {
            symbols[0].uri.clone()
        }
        _ => {
            task.abort();
            let _ = task.await;
            return;
        }
    };

    send(
        &mut client,
        &ClientMessage::QueryReferences {
            symbol_uri: sym_uri,
            limit: Some(20),
        },
    )
    .await
    .unwrap();

    let resp = recv(&mut client).await.unwrap();
    assert!(
        matches!(resp, ServerMessage::ReferencesResult { .. }),
        "expected ReferencesResult, got {resp:?}"
    );

    task.abort();
    let _ = task.await;
}

// ─── Annotations survive daemon restart ──────────────────────────────────────

#[tokio::test]
async fn daemon_annotations_survive_restart() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("lip_annot.sock");
    let sym_uri = "lip://local/test@0.1/annot.rs#annotated_fn";

    // ── Write annotation ─────────────────────────────────────────────────────
    {
        let daemon = LipDaemon::new(&socket_path);
        let task = tokio::spawn(async move { daemon.run().await.ok() });
        tokio::time::sleep(Duration::from_millis(20)).await;

        let mut client = UnixStream::connect(&socket_path).await.unwrap();
        send(
            &mut client,
            &ClientMessage::AnnotationSet {
                symbol_uri: sym_uri.into(),
                key: "team:owner".into(),
                value: "platform".into(),
                author_id: "human:test".into(),
            },
        )
        .await
        .unwrap();
        let _ = recv(&mut client).await.unwrap(); // AnnotationAck

        task.abort();
        let _ = task.await;
    }

    tokio::time::sleep(Duration::from_millis(10)).await;

    // ── Restart and read annotation back ─────────────────────────────────────
    {
        let daemon = LipDaemon::new(&socket_path);
        let task = tokio::spawn(async move { daemon.run().await.ok() });
        tokio::time::sleep(Duration::from_millis(30)).await;

        let mut client = UnixStream::connect(&socket_path).await.unwrap();
        send(
            &mut client,
            &ClientMessage::AnnotationGet {
                symbol_uri: sym_uri.into(),
                key: "team:owner".into(),
            },
        )
        .await
        .unwrap();

        let resp = recv(&mut client).await.unwrap();
        match resp {
            ServerMessage::AnnotationValue { value } => {
                assert_eq!(
                    value.as_deref(),
                    Some("platform"),
                    "annotation lost across daemon restart"
                );
            }
            other => panic!("expected AnnotationValue, got {other:?}"),
        }

        task.abort();
        let _ = task.await;
    }
}

// ─── stream_context (LIP 2.1.0) ──────────────────────────────────────────────

async fn recv_stream_frame(stream: &mut UnixStream) -> anyhow::Result<ServerMessage> {
    // Filter push notifications (`IndexChanged`, `SymbolUpgraded`) that may
    // have been queued from earlier upserts.
    recv(stream).await
}

#[tokio::test]
async fn stream_context_zero_budget_terminates_immediately() {
    use lip_core::query_graph::types::EndStreamReason;
    use lip_core::schema::OwnedRange;

    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("lip_stream_zero.sock");
    let daemon = LipDaemon::new(&socket);
    let task = tokio::spawn(async move { daemon.run().await.ok() });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let mut client = UnixStream::connect(&socket).await.expect("connect");
    let uri = "lip://local/test@0.1/budget.rs";
    let source = "pub fn foo() {}\n";
    send(
        &mut client,
        &ClientMessage::Delta {
            seq: 1,
            action: Action::Upsert,
            document: make_doc(uri, source),
        },
    )
    .await
    .unwrap();
    let _ = recv(&mut client).await.unwrap();

    send(
        &mut client,
        &ClientMessage::StreamContext {
            file_uri: uri.into(),
            cursor_position: OwnedRange {
                start_line: 0,
                start_char: 0,
                end_line: 0,
                end_char: 0,
            },
            max_tokens: 0,
            model: None,
        },
    )
    .await
    .unwrap();

    let frame = recv_stream_frame(&mut client).await.unwrap();
    match frame {
        ServerMessage::EndStream {
            reason, emitted, ..
        } => {
            assert_eq!(reason, EndStreamReason::BudgetReached);
            assert_eq!(emitted, 0);
        }
        other => panic!("expected EndStream first frame, got {other:?}"),
    }

    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn stream_context_cursor_out_of_range_errors() {
    use lip_core::query_graph::types::EndStreamReason;
    use lip_core::schema::OwnedRange;

    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("lip_stream_oob.sock");
    let daemon = LipDaemon::new(&socket);
    let task = tokio::spawn(async move { daemon.run().await.ok() });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let mut client = UnixStream::connect(&socket).await.expect("connect");
    let uri = "lip://local/test@0.1/oob.rs";
    let source = "pub fn foo() {}\n"; // 1 line
    send(
        &mut client,
        &ClientMessage::Delta {
            seq: 1,
            action: Action::Upsert,
            document: make_doc(uri, source),
        },
    )
    .await
    .unwrap();
    let _ = recv(&mut client).await.unwrap();

    send(
        &mut client,
        &ClientMessage::StreamContext {
            file_uri: uri.into(),
            cursor_position: OwnedRange {
                start_line: 9999,
                start_char: 0,
                end_line: 9999,
                end_char: 0,
            },
            max_tokens: 4096,
            model: None,
        },
    )
    .await
    .unwrap();

    let frame = recv_stream_frame(&mut client).await.unwrap();
    match frame {
        ServerMessage::EndStream { reason, error, .. } => {
            assert_eq!(reason, EndStreamReason::CursorOutOfRange);
            // Message carries the actual line count so callers can
            // surface a useful error without parsing the reason string.
            let msg = error.as_deref().unwrap_or("");
            assert!(
                msg.contains("cursor line 9999") && msg.contains("1 lines"),
                "unexpected error message: {msg:?}"
            );
        }
        other => panic!("expected EndStream(CursorOutOfRange), got {other:?}"),
    }

    task.abort();
    let _ = task.await;
}

/// A cursor against a URI the daemon has never seen must terminate with
/// `FileNotIndexed`, not `CursorOutOfRange`. The two were collapsed
/// onto `Error` + a free-form string before; splitting lets CKB show
/// "upsert the file first" vs. "your coordinates are bad."
#[tokio::test]
async fn stream_context_unknown_uri_reports_file_not_indexed() {
    use lip_core::query_graph::types::EndStreamReason;
    use lip_core::schema::OwnedRange;

    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("lip_stream_unknown.sock");
    let daemon = LipDaemon::new(&socket);
    let task = tokio::spawn(async move { daemon.run().await.ok() });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let mut client = UnixStream::connect(&socket).await.expect("connect");

    // Do NOT upsert anything. The daemon has no record of this URI.
    send(
        &mut client,
        &ClientMessage::StreamContext {
            file_uri: "lip://local/never/indexed.rs".into(),
            cursor_position: OwnedRange {
                start_line: 0,
                start_char: 0,
                end_line: 0,
                end_char: 0,
            },
            max_tokens: 4096,
            model: None,
        },
    )
    .await
    .unwrap();

    let frame = recv_stream_frame(&mut client).await.unwrap();
    match frame {
        ServerMessage::EndStream { reason, error, .. } => {
            assert_eq!(reason, EndStreamReason::FileNotIndexed);
            assert!(
                error
                    .as_deref()
                    .unwrap_or("")
                    .contains("not in the daemon index"),
                "expected daemon-index error message, got {error:?}"
            );
        }
        other => panic!("expected EndStream(FileNotIndexed), got {other:?}"),
    }

    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn embed_text_without_endpoint_returns_error() {
    // No `LIP_EMBEDDING_URL` set in the test process → embedding client is None
    // → daemon returns the documented configuration error.
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("lip_embed_text.sock");
    let daemon = LipDaemon::new(&socket);
    let task = tokio::spawn(async move { daemon.run().await.ok() });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let mut client = UnixStream::connect(&socket).await.expect("connect");
    send(
        &mut client,
        &ClientMessage::EmbedText {
            text: "verify token expiry".into(),
            model: None,
        },
    )
    .await
    .unwrap();

    let resp = recv(&mut client).await.unwrap();
    match resp {
        ServerMessage::Error { message, code } => {
            assert!(
                message.contains("LIP_EMBEDDING_URL"),
                "expected configuration error, got {message:?}"
            );
            assert_eq!(
                code,
                ErrorCode::EmbeddingNotConfigured,
                "expected EmbeddingNotConfigured code, got {code:?}"
            );
        }
        ServerMessage::EmbedTextResult { vector, .. } => {
            // If a real embedding endpoint is configured in CI, fall through.
            assert!(!vector.is_empty(), "vector should be non-empty");
        }
        other => panic!("expected Error or EmbedTextResult, got {other:?}"),
    }

    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn stream_context_handshake_advertises_v2() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("lip_stream_hs.sock");
    let daemon = LipDaemon::new(&socket);
    let task = tokio::spawn(async move { daemon.run().await.ok() });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let mut client = UnixStream::connect(&socket).await.expect("connect");
    send(
        &mut client,
        &ClientMessage::Handshake {
            client_version: Some("test".into()),
        },
    )
    .await
    .unwrap();
    let resp = recv(&mut client).await.unwrap();
    match resp {
        ServerMessage::HandshakeResult {
            protocol_version, ..
        } => assert_eq!(protocol_version, 2),
        other => panic!("expected HandshakeResult, got {other:?}"),
    }

    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn handshake_advertises_supported_messages() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("lip_caps.sock");
    let daemon = LipDaemon::new(&socket);
    let task = tokio::spawn(async move { daemon.run().await.ok() });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let mut client = UnixStream::connect(&socket).await.expect("connect");
    send(
        &mut client,
        &ClientMessage::Handshake {
            client_version: Some("test".into()),
        },
    )
    .await
    .unwrap();

    let resp = recv(&mut client).await.unwrap();
    match resp {
        ServerMessage::HandshakeResult {
            supported_messages, ..
        } => {
            assert!(supported_messages.contains(&"handshake".to_string()));
            assert!(supported_messages.contains(&"stream_context".to_string()));
            assert!(supported_messages.contains(&"embed_text".to_string()));
            assert!(!supported_messages.contains(&"nonexistent_message".to_string()));
        }
        other => panic!("expected HandshakeResult, got {other:?}"),
    }

    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn unknown_variant_returns_unknown_message_and_keeps_connection() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket = dir.path().join("lip_unknown.sock");
    let daemon = LipDaemon::new(&socket);
    let task = tokio::spawn(async move { daemon.run().await.ok() });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let mut client = UnixStream::connect(&socket).await.expect("connect");

    // Hand-craft an envelope with an unknown `type` tag — the daemon should
    // recognise this as recoverable and reply with `UnknownMessage` rather
    // than closing the socket.
    let bogus = serde_json::json!({
        "type": "summon_kraken",
        "payload": {"when": "at_dawn"},
    });
    let body = serde_json::to_vec(&bogus).unwrap();
    client
        .write_all(&(body.len() as u32).to_be_bytes())
        .await
        .unwrap();
    client.write_all(&body).await.unwrap();

    let resp = recv(&mut client).await.unwrap();
    match resp {
        ServerMessage::UnknownMessage {
            message_type,
            supported,
        } => {
            assert_eq!(message_type.as_deref(), Some("summon_kraken"));
            assert!(supported.contains(&"handshake".to_string()));
        }
        other => panic!("expected UnknownMessage, got {other:?}"),
    }

    // Connection must still be usable: send a Handshake after the error.
    send(
        &mut client,
        &ClientMessage::Handshake {
            client_version: Some("test".into()),
        },
    )
    .await
    .unwrap();
    match recv(&mut client).await.unwrap() {
        ServerMessage::HandshakeResult { .. } => {}
        other => panic!("expected HandshakeResult after recovery, got {other:?}"),
    }

    task.abort();
    let _ = task.await;
}

// ─── SCIP import: pre-computed symbols via Delta ─────────────────────────────

/// Regression test for the SCIP import path. When a client sends a Delta with
/// `source_text: None` and pre-computed `symbols` + `occurrences`, the daemon
/// must store them verbatim (via `upsert_file_precomputed`) and make them
/// queryable through both `WorkspaceSymbols` and `QueryDefinition`.
#[tokio::test]
async fn scip_import_precomputed_symbols_searchable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("lip_scip.sock");

    let daemon = LipDaemon::new(&socket_path);
    let task = tokio::spawn(async move { daemon.run().await.ok() });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let mut client = UnixStream::connect(&socket_path).await.expect("connect");

    // ── Build a pre-computed document (SCIP-style: no source_text) ───────────
    let uri = "lip://local/dep@1.0/scip_mod.rs";
    let symbol_uri = format!("{uri}#ScipWidget");

    let doc = OwnedDocument {
        uri: uri.to_owned(),
        content_hash: "cafebabe01234567".to_owned(),
        language: "rust".to_owned(),
        symbols: vec![OwnedSymbolInfo {
            uri: symbol_uri.clone(),
            display_name: "ScipWidget".to_owned(),
            kind: SymbolKind::Class,
            documentation: Some("A widget from SCIP import.".to_owned()),
            signature: Some("pub struct ScipWidget".to_owned()),
            confidence_score: 100,
            relationships: vec![],
            runtime_p99_ms: None,
            call_rate_per_s: None,
            taint_labels: vec![],
            blast_radius: 0,
            is_exported: true,
            ..Default::default()
        }],
        occurrences: vec![OwnedOccurrence {
            symbol_uri: symbol_uri.clone(),
            range: OwnedRange {
                start_line: 0,
                start_char: 11,
                end_line: 0,
                end_char: 21,
            },
            confidence_score: 100,
            role: Role::Definition,
            override_doc: None,
            kind: lip_core::schema::ReferenceKind::Unknown,
            is_test: false,
        }],
        merkle_path: uri.to_owned(),
        edges: vec![],
        source_text: None, // <-- key: SCIP imports have no source
    };

    // ── Send the Delta ───────────────────────────────────────────────────────
    send(
        &mut client,
        &ClientMessage::Delta {
            seq: 100,
            action: Action::Upsert,
            document: doc,
        },
    )
    .await
    .expect("send scip delta");

    let resp = recv(&mut client).await.expect("recv scip delta ack");
    match resp {
        ServerMessage::DeltaAck { seq, accepted, .. } => {
            assert_eq!(seq, 100);
            assert!(accepted, "daemon rejected pre-computed delta");
        }
        other => panic!("expected DeltaAck, got {other:?}"),
    }

    // ── WorkspaceSymbols: the pre-computed symbol must be discoverable ───────
    send(
        &mut client,
        &ClientMessage::QueryWorkspaceSymbols {
            kind_filter: None,
            scope: None,
            modifier_filter: None,
            query: "ScipWidget".to_owned(),
            limit: Some(10),
        },
    )
    .await
    .expect("send workspace symbols query");

    let resp = recv(&mut client).await.expect("recv workspace symbols");
    match resp {
        ServerMessage::WorkspaceSymbolsResult { symbols, .. } => {
            assert!(
                !symbols.is_empty(),
                "expected ScipWidget in workspace symbols, got none"
            );
            assert!(
                symbols.iter().any(|s| s.display_name == "ScipWidget"),
                "ScipWidget not found in results: {symbols:?}"
            );
        }
        other => panic!("expected WorkspaceSymbolsResult, got {other:?}"),
    }

    // ── QueryDefinition: the Definition-role occurrence must resolve ─────────
    send(
        &mut client,
        &ClientMessage::QueryDefinition {
            uri: uri.to_owned(),
            line: 0,
            col: 15, // inside the occurrence range [11..21]
        },
    )
    .await
    .expect("send query definition");

    let resp = recv(&mut client).await.expect("recv definition result");
    match resp {
        ServerMessage::DefinitionResult {
            symbol,
            location_uri,
            ..
        } => {
            assert!(
                symbol.is_some(),
                "expected symbol info for ScipWidget, got None"
            );
            let sym = symbol.unwrap();
            assert_eq!(sym.display_name, "ScipWidget");
            assert_eq!(
                location_uri.as_deref(),
                Some(uri),
                "definition should resolve to the same file"
            );
        }
        other => panic!("expected DefinitionResult, got {other:?}"),
    }

    // ── Cleanup ──────────────────────────────────────────────────────────────
    task.abort();
    let _ = task.await;
}

// ─── v2.3 rich metadata end-to-end ───────────────────────────────────────────

/// Upsert a Rust file with source_text, then query WorkspaceSymbols and verify
/// the Tier-1 extractor's v2.3 structural fields (modifiers, visibility,
/// container_name, signature_normalized) survive the daemon's storage and
/// response serialization round-trip. Tier-1 produces `extraction_tier = Tier1`
/// and leaves `modifiers_source = None` (that field is reserved for SCIP).
#[tokio::test]
async fn daemon_tier1_emits_v23_metadata() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("lip_v23_tier1.sock");

    let daemon = LipDaemon::new(&socket_path);
    let task = tokio::spawn(async move { daemon.run().await.ok() });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let mut client = UnixStream::connect(&socket_path).await.expect("connect");

    let source = "\
pub struct Svc;
impl Svc {
    pub async fn handle(&self, x: i32) -> i32 { x }
}
";
    let uri = "lip://local/v23@0.1/svc.rs";
    send(
        &mut client,
        &ClientMessage::Delta {
            seq: 1,
            action: Action::Upsert,
            document: make_doc(uri, source),
        },
    )
    .await
    .expect("send upsert");
    let _ = recv(&mut client).await.expect("recv ack");

    send(
        &mut client,
        &ClientMessage::QueryWorkspaceSymbols {
            kind_filter: None,
            scope: None,
            modifier_filter: None,
            query: "handle".to_owned(),
            limit: Some(10),
        },
    )
    .await
    .expect("send workspace query");

    let resp = recv(&mut client).await.expect("recv workspace symbols");
    let handle = match resp {
        ServerMessage::WorkspaceSymbolsResult { symbols, .. } => symbols
            .into_iter()
            .find(|s| s.display_name == "handle")
            .expect("expected 'handle' method in workspace symbols"),
        other => panic!("expected WorkspaceSymbolsResult, got {other:?}"),
    };

    assert_eq!(handle.extraction_tier, ExtractionTier::Tier1);
    assert_eq!(
        handle.modifiers_source, None,
        "Tier-1 must not set modifiers_source; that field is reserved for SCIP"
    );
    assert_eq!(handle.visibility, Some(Visibility::Public));
    assert_eq!(handle.container_name.as_deref(), Some("Svc"));
    assert!(
        handle.modifiers.iter().any(|m| m == "pub"),
        "expected `pub` modifier, got {:?}",
        handle.modifiers
    );
    assert!(
        handle.modifiers.iter().any(|m| m == "async"),
        "expected `async` modifier, got {:?}",
        handle.modifiers
    );
    assert!(
        handle
            .signature_normalized
            .as_deref()
            .map(|s| s.contains("fn handle"))
            .unwrap_or(false),
        "expected normalized signature containing `fn handle`, got {:?}",
        handle.signature_normalized
    );

    task.abort();
    let _ = task.await;
}

/// Upsert a Delta carrying pre-computed symbols with v2.3 fields populated (as
/// a SCIP-style importer would produce) and verify every structural and
/// telemetry field survives the daemon's storage and query serialization.
///
/// This is the tightest available check that the daemon's write path does not
/// drop `modifiers`, `visibility`, `container_name`, `signature_normalized`,
/// `extraction_tier`, or `modifiers_source`.
#[tokio::test]
async fn daemon_precomputed_preserves_v23_metadata() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("lip_v23_scip.sock");

    let daemon = LipDaemon::new(&socket_path);
    let task = tokio::spawn(async move { daemon.run().await.ok() });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let mut client = UnixStream::connect(&socket_path).await.expect("connect");

    let uri = "lip://local/scip@1.0/imported.rs";
    let sym_uri = format!("{uri}#RichSym");

    let sym = OwnedSymbolInfo {
        uri: sym_uri.clone(),
        display_name: "RichSym".to_owned(),
        kind: SymbolKind::Function,
        documentation: Some("A symbol carrying full v2.3 metadata.".to_owned()),
        signature: Some("pub async fn RichSym(x: i32) -> Bar".to_owned()),
        signature_normalized: Some("pub async fn RichSym(_: i32) -> Bar".to_owned()),
        modifiers: vec!["pub".to_owned(), "async".to_owned()],
        visibility: Some(Visibility::Public),
        visibility_confidence: Some(1.0),
        container_name: Some("RichContainer".to_owned()),
        extraction_tier: ExtractionTier::Tier3Scip,
        modifiers_source: Some(ModifiersSource::PrefixParse),
        confidence_score: 100,
        is_exported: true,
        ..Default::default()
    };

    let doc = OwnedDocument {
        uri: uri.to_owned(),
        content_hash: "deadbeef".to_owned(),
        language: "rust".to_owned(),
        symbols: vec![sym],
        occurrences: vec![OwnedOccurrence {
            symbol_uri: sym_uri.clone(),
            range: OwnedRange {
                start_line: 0,
                start_char: 0,
                end_line: 0,
                end_char: 7,
            },
            confidence_score: 100,
            role: Role::Definition,
            override_doc: None,
            kind: lip_core::schema::ReferenceKind::Unknown,
            is_test: false,
        }],
        merkle_path: uri.to_owned(),
        edges: vec![],
        source_text: None, // SCIP-style: no source
    };

    send(
        &mut client,
        &ClientMessage::Delta {
            seq: 7,
            action: Action::Upsert,
            document: doc,
        },
    )
    .await
    .expect("send delta");
    let _ = recv(&mut client).await.expect("recv ack");

    send(
        &mut client,
        &ClientMessage::QueryWorkspaceSymbols {
            kind_filter: None,
            scope: None,
            modifier_filter: None,
            query: "RichSym".to_owned(),
            limit: Some(10),
        },
    )
    .await
    .expect("send query");

    let resp = recv(&mut client).await.expect("recv workspace symbols");
    let got = match resp {
        ServerMessage::WorkspaceSymbolsResult { symbols, .. } => symbols
            .into_iter()
            .find(|s| s.display_name == "RichSym")
            .expect("expected RichSym in workspace symbols"),
        other => panic!("expected WorkspaceSymbolsResult, got {other:?}"),
    };

    assert_eq!(got.extraction_tier, ExtractionTier::Tier3Scip);
    assert_eq!(got.modifiers_source, Some(ModifiersSource::PrefixParse));
    assert_eq!(got.visibility, Some(Visibility::Public));
    assert_eq!(got.visibility_confidence, Some(1.0));
    assert_eq!(got.container_name.as_deref(), Some("RichContainer"));
    assert_eq!(got.modifiers, vec!["pub".to_owned(), "async".to_owned()]);
    assert_eq!(
        got.signature_normalized.as_deref(),
        Some("pub async fn RichSym(_: i32) -> Bar")
    );

    task.abort();
    let _ = task.await;
}

/// End-to-end v2.3 reference-kind test. Upserts a Rust file with a function
/// definition and a call site, then verifies the daemon's ReferencesResult
/// carries `kind = Call` on the reference occurrence produced by Tier-1.
#[tokio::test]
async fn daemon_tier1_call_occurrence_has_ref_kind_call() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("lip_v23_refkind.sock");

    let daemon = LipDaemon::new(&socket_path);
    let task = tokio::spawn(async move { daemon.run().await.ok() });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let mut client = UnixStream::connect(&socket_path).await.expect("connect");

    let source = "\
pub fn callee() {}
pub fn caller() {
    callee();
}
";
    let uri = "lip://local/v23refkind@0.1/src.rs";
    send(
        &mut client,
        &ClientMessage::Delta {
            seq: 1,
            action: Action::Upsert,
            document: make_doc(uri, source),
        },
    )
    .await
    .expect("send upsert");
    let _ = recv(&mut client).await.expect("recv ack");

    send(
        &mut client,
        &ClientMessage::QueryWorkspaceSymbols {
            kind_filter: None,
            scope: None,
            modifier_filter: None,
            query: "callee".to_owned(),
            limit: Some(10),
        },
    )
    .await
    .expect("send workspace query");
    let callee_uri = match recv(&mut client).await.expect("recv workspace") {
        ServerMessage::WorkspaceSymbolsResult { symbols, .. } => symbols
            .into_iter()
            .find(|s| s.display_name == "callee")
            .expect("expected 'callee' in workspace symbols")
            .uri,
        other => panic!("expected WorkspaceSymbolsResult, got {other:?}"),
    };

    send(
        &mut client,
        &ClientMessage::QueryReferences {
            symbol_uri: callee_uri.clone(),
            limit: Some(10),
        },
    )
    .await
    .expect("send refs query");
    let refs = match recv(&mut client).await.expect("recv refs") {
        ServerMessage::ReferencesResult { occurrences } => occurrences,
        other => panic!("expected ReferencesResult, got {other:?}"),
    };

    let call_ref = refs
        .iter()
        .find(|o| o.role == Role::Reference)
        .expect("expected at least one reference occurrence for `callee`");
    assert_eq!(
        call_ref.kind,
        ReferenceKind::Call,
        "Tier-1 must tag `callee()` with ReferenceKind::Call; got {:?}",
        call_ref.kind
    );
    assert!(
        !call_ref.is_test,
        "non-test file must not set is_test; got {:?} for uri {}",
        call_ref.is_test, uri
    );

    task.abort();
    let _ = task.await;
}

/// Upsert a file whose URI contains `/tests/` and verify Tier-1 stamps
/// `is_test = true` on every occurrence — the down-rank signal for CKB.
#[tokio::test]
async fn daemon_tier1_test_file_stamps_is_test() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("lip_v23_istest.sock");

    let daemon = LipDaemon::new(&socket_path);
    let task = tokio::spawn(async move { daemon.run().await.ok() });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let mut client = UnixStream::connect(&socket_path).await.expect("connect");

    let source = "pub fn helper() { helper(); }";
    let uri = "lip://local/myproj@0.1/tests/integration.rs";
    send(
        &mut client,
        &ClientMessage::Delta {
            seq: 1,
            action: Action::Upsert,
            document: make_doc(uri, source),
        },
    )
    .await
    .expect("send upsert");
    let _ = recv(&mut client).await.expect("recv ack");

    send(
        &mut client,
        &ClientMessage::QueryWorkspaceSymbols {
            kind_filter: None,
            scope: None,
            modifier_filter: None,
            query: "helper".to_owned(),
            limit: Some(10),
        },
    )
    .await
    .expect("send workspace query");
    let helper_uri = match recv(&mut client).await.expect("recv workspace") {
        ServerMessage::WorkspaceSymbolsResult { symbols, .. } => symbols
            .into_iter()
            .find(|s| s.display_name == "helper")
            .expect("expected 'helper' in workspace symbols")
            .uri,
        other => panic!("expected WorkspaceSymbolsResult, got {other:?}"),
    };

    send(
        &mut client,
        &ClientMessage::QueryReferences {
            symbol_uri: helper_uri,
            limit: Some(10),
        },
    )
    .await
    .expect("send refs query");
    let refs = match recv(&mut client).await.expect("recv refs") {
        ServerMessage::ReferencesResult { occurrences } => occurrences,
        other => panic!("expected ReferencesResult, got {other:?}"),
    };

    assert!(
        !refs.is_empty(),
        "expected at least one reference occurrence"
    );
    for o in &refs {
        assert!(
            o.is_test,
            "Tier-1 must stamp is_test on occurrences from /tests/ files; got false"
        );
    }

    task.abort();
    let _ = task.await;
}

/// QueryBlastRadiusSymbol (v2.3 Feature #3): single-symbol analogue of
/// QueryBlastRadiusBatch. Upsert two Rust files where file B calls a function
/// defined in file A, then ask the daemon for A's function's blast radius and
/// verify file B appears in `affected_files`. Also verify the `None` path for
/// unknown symbols.
#[tokio::test]
async fn daemon_query_blast_radius_symbol() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("lip_v23_br_symbol.sock");

    let daemon = LipDaemon::new(&socket_path);
    let task = tokio::spawn(async move { daemon.run().await.ok() });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let mut client = UnixStream::connect(&socket_path).await.expect("connect");

    let a_uri = "lip://local/brsym@0.1/a.rs";
    let a_src = "pub fn victim() {}";
    send(
        &mut client,
        &ClientMessage::Delta {
            seq: 1,
            action: Action::Upsert,
            document: make_doc(a_uri, a_src),
        },
    )
    .await
    .expect("send a");
    let _ = recv(&mut client).await.expect("ack a");

    let b_uri = "lip://local/brsym@0.1/b.rs";
    let b_src = "\
pub fn caller() {
    crate::a::victim();
}
";
    send(
        &mut client,
        &ClientMessage::Delta {
            seq: 2,
            action: Action::Upsert,
            document: make_doc(b_uri, b_src),
        },
    )
    .await
    .expect("send b");
    let _ = recv(&mut client).await.expect("ack b");

    send(
        &mut client,
        &ClientMessage::QueryWorkspaceSymbols {
            kind_filter: None,
            scope: None,
            modifier_filter: None,
            query: "victim".to_owned(),
            limit: Some(10),
        },
    )
    .await
    .expect("send workspace");
    let victim_uri = match recv(&mut client).await.expect("recv workspace") {
        ServerMessage::WorkspaceSymbolsResult { symbols, .. } => symbols
            .into_iter()
            .find(|s| s.display_name == "victim")
            .expect("expected `victim` in workspace")
            .uri,
        other => panic!("expected WorkspaceSymbolsResult, got {other:?}"),
    };

    send(
        &mut client,
        &ClientMessage::QueryBlastRadiusSymbol {
            symbol_uri: victim_uri.clone(),
            min_score: None,
        },
    )
    .await
    .expect("send br symbol");
    let enriched = match recv(&mut client).await.expect("recv br symbol") {
        ServerMessage::BlastRadiusSymbolResult { result } => {
            result.expect("expected Some(EnrichedBlastRadius) for indexed symbol")
        }
        other => panic!("expected BlastRadiusSymbolResult, got {other:?}"),
    };

    // The file that defines `victim` — the enrichment's anchor.
    assert_eq!(enriched.file_uri, a_uri);
    assert!(
        enriched.static_result.affected_files.iter().any(|f| f == b_uri),
        "expected caller file {b_uri} in affected_files, got {:?}",
        enriched.static_result.affected_files
    );
    // min_score was None — enrichment must be skipped.
    assert!(
        enriched.semantic_items.is_empty(),
        "min_score = None must skip semantic enrichment; got {:?}",
        enriched.semantic_items
    );

    // Unknown symbol URI → None (not an error).
    send(
        &mut client,
        &ClientMessage::QueryBlastRadiusSymbol {
            symbol_uri: "lip://local/does/not/exist#nope".to_owned(),
            min_score: None,
        },
    )
    .await
    .expect("send unknown br symbol");
    match recv(&mut client).await.expect("recv unknown br symbol") {
        ServerMessage::BlastRadiusSymbolResult { result } => {
            assert!(result.is_none(), "unknown symbol must return None result")
        }
        other => panic!("expected BlastRadiusSymbolResult, got {other:?}"),
    }

    task.abort();
    let _ = task.await;
}

/// QueryOutgoingCalls (v2.3 Feature #4): forward call-graph BFS. Upsert a
/// single file containing an A→B→C call chain and verify that depth=2
/// returns both edges, while depth=1 only returns A→B.
#[tokio::test]
async fn daemon_query_outgoing_calls_depth() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("lip_v23_outgoing.sock");

    let daemon = LipDaemon::new(&socket_path);
    let task = tokio::spawn(async move { daemon.run().await.ok() });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let mut client = UnixStream::connect(&socket_path).await.expect("connect");

    let uri = "lip://local/og@0.1/chain.rs";
    let src = "\
fn a() { b(); }
fn b() { c(); }
fn c() {}
";
    send(
        &mut client,
        &ClientMessage::Delta {
            seq: 1,
            action: Action::Upsert,
            document: make_doc(uri, src),
        },
    )
    .await
    .expect("send upsert");
    let _ = recv(&mut client).await.expect("recv ack");

    // Resolve `a`'s URI via workspace symbols.
    send(
        &mut client,
        &ClientMessage::QueryWorkspaceSymbols {
            kind_filter: None,
            scope: None,
            modifier_filter: None,
            query: "a".to_owned(),
            limit: Some(20),
        },
    )
    .await
    .expect("send workspace");
    let a_uri = match recv(&mut client).await.expect("recv workspace") {
        ServerMessage::WorkspaceSymbolsResult { symbols, .. } => symbols
            .into_iter()
            .find(|s| s.display_name == "a")
            .expect("expected `a` in workspace symbols")
            .uri,
        other => panic!("expected WorkspaceSymbolsResult, got {other:?}"),
    };

    // Depth 2: A→B and B→C must both be present.
    send(
        &mut client,
        &ClientMessage::QueryOutgoingCalls {
            symbol_uri: a_uri.clone(),
            depth: 2,
        },
    )
    .await
    .expect("send outgoing depth=2");
    let (edges, truncated) = match recv(&mut client).await.expect("recv depth=2") {
        ServerMessage::OutgoingCallsResult { edges, truncated } => (edges, truncated),
        other => panic!("expected OutgoingCallsResult, got {other:?}"),
    };
    assert!(!truncated, "chain is tiny; truncated must be false");
    assert!(
        edges.iter().any(|e| e.from_uri == a_uri && e.to_uri.ends_with("#b")),
        "expected A→B edge; got {edges:?}",
    );
    assert!(
        edges.iter().any(|e| e.from_uri.ends_with("#b") && e.to_uri.ends_with("#c")),
        "expected B→C edge at depth=2; got {edges:?}",
    );

    // Depth 1: B→C must be absent.
    send(
        &mut client,
        &ClientMessage::QueryOutgoingCalls {
            symbol_uri: a_uri.clone(),
            depth: 1,
        },
    )
    .await
    .expect("send outgoing depth=1");
    let (edges1, _) = match recv(&mut client).await.expect("recv depth=1") {
        ServerMessage::OutgoingCallsResult { edges, truncated } => (edges, truncated),
        other => panic!("expected OutgoingCallsResult, got {other:?}"),
    };
    assert!(
        edges1.iter().any(|e| e.to_uri.ends_with("#b")),
        "depth=1 must still include A→B; got {edges1:?}",
    );
    assert!(
        !edges1.iter().any(|e| e.to_uri.ends_with("#c")),
        "depth=1 must exclude B→C; got {edges1:?}",
    );

    task.abort();
    let _ = task.await;
}

/// Feature #5c: ranked workspace symbols.
///
/// Verifies the four new v2.3 behaviors on `QueryWorkspaceSymbols`:
/// 1. Ranking tiers: Exact (1.0) > Prefix (0.8), and `ranked` parallels `symbols`.
/// 2. `kind_filter` narrows the result set to the requested `SymbolKind`s.
/// 3. `scope` restricts to symbols whose def-file URI starts with the prefix.
/// 4. `modifier_filter` restricts to symbols carrying at least one listed modifier.
/// 5. An empty query returns `ranked = []` (preserves pre-v2.3 semantics).
#[tokio::test]
async fn daemon_workspace_symbols_v23_filters_and_ranking() {
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("lip_v23_ranked.sock");

    let daemon = LipDaemon::new(&socket_path);
    let task = tokio::spawn(async move { daemon.run().await.ok() });
    tokio::time::sleep(Duration::from_millis(20)).await;

    let mut client = UnixStream::connect(&socket_path).await.expect("connect");

    // File A under scope `lip://local/srv`: `Handler` struct + async fn `handle`.
    let srv_uri = "lip://local/srv@0.1/a.rs";
    let srv_src = "\
pub struct Handler;
pub async fn handle() {}
";
    send(
        &mut client,
        &ClientMessage::Delta {
            seq: 1,
            action: Action::Upsert,
            document: make_doc(srv_uri, srv_src),
        },
    )
    .await
    .expect("send srv upsert");
    let _ = recv(&mut client).await.expect("recv srv ack");

    // File B under scope `lip://local/cli`: `HandlerFactory` struct.
    let cli_uri = "lip://local/cli@0.1/b.rs";
    let cli_src = "pub struct HandlerFactory;\n";
    send(
        &mut client,
        &ClientMessage::Delta {
            seq: 2,
            action: Action::Upsert,
            document: make_doc(cli_uri, cli_src),
        },
    )
    .await
    .expect("send cli upsert");
    let _ = recv(&mut client).await.expect("recv cli ack");

    // (1) Ranking tiers — query "Handler" hits Exact on `Handler` and Prefix on `HandlerFactory`.
    send(
        &mut client,
        &ClientMessage::QueryWorkspaceSymbols {
            kind_filter: None,
            scope: None,
            modifier_filter: None,
            query: "Handler".to_owned(),
            limit: Some(50),
        },
    )
    .await
    .expect("send ranked query");
    let (symbols, ranked) = match recv(&mut client).await.expect("recv ranked") {
        ServerMessage::WorkspaceSymbolsResult {
            symbols, ranked, ..
        } => (symbols, ranked),
        other => panic!("expected WorkspaceSymbolsResult, got {other:?}"),
    };
    assert_eq!(
        symbols.len(),
        ranked.len(),
        "ranked must parallel symbols; got {} vs {}",
        symbols.len(),
        ranked.len(),
    );
    let exact = ranked
        .iter()
        .zip(symbols.iter())
        .find(|(_, s)| s.display_name == "Handler")
        .map(|(r, _)| r)
        .expect("expected Handler in ranked list");
    assert!(
        matches!(exact.match_type, lip_core::query_graph::types::MatchType::Exact),
        "Handler should be Exact match, got {:?}",
        exact.match_type,
    );
    assert!((exact.score - 1.0).abs() < 1e-6, "exact score must be 1.0");
    let prefix = ranked
        .iter()
        .zip(symbols.iter())
        .find(|(_, s)| s.display_name == "HandlerFactory")
        .map(|(r, _)| r)
        .expect("expected HandlerFactory in ranked list");
    assert!(
        matches!(prefix.match_type, lip_core::query_graph::types::MatchType::Prefix),
        "HandlerFactory should be Prefix match, got {:?}",
        prefix.match_type,
    );
    assert!(
        (prefix.score - 0.8).abs() < 1e-6,
        "prefix score must be 0.8"
    );
    // Sorted: exact before prefix.
    let exact_idx = symbols
        .iter()
        .position(|s| s.display_name == "Handler")
        .unwrap();
    let prefix_idx = symbols
        .iter()
        .position(|s| s.display_name == "HandlerFactory")
        .unwrap();
    assert!(
        exact_idx < prefix_idx,
        "Exact must sort before Prefix; got exact={exact_idx}, prefix={prefix_idx}",
    );

    // (2) kind_filter — only Function symbols.
    send(
        &mut client,
        &ClientMessage::QueryWorkspaceSymbols {
            kind_filter: Some(vec![SymbolKind::Function]),
            scope: None,
            modifier_filter: None,
            query: String::new(),
            limit: Some(50),
        },
    )
    .await
    .expect("send kind query");
    let symbols = match recv(&mut client).await.expect("recv kind") {
        ServerMessage::WorkspaceSymbolsResult { symbols, .. } => symbols,
        other => panic!("expected WorkspaceSymbolsResult, got {other:?}"),
    };
    assert!(!symbols.is_empty(), "expected at least one Function");
    assert!(
        symbols.iter().all(|s| s.kind == SymbolKind::Function),
        "kind_filter violated; got kinds {:?}",
        symbols.iter().map(|s| s.kind).collect::<Vec<_>>(),
    );
    assert!(
        symbols.iter().any(|s| s.display_name == "handle"),
        "expected `handle` fn, got {:?}",
        symbols.iter().map(|s| &s.display_name).collect::<Vec<_>>(),
    );

    // (3) scope — only symbols defined under `lip://local/cli`.
    send(
        &mut client,
        &ClientMessage::QueryWorkspaceSymbols {
            kind_filter: None,
            scope: Some("lip://local/cli".to_owned()),
            modifier_filter: None,
            query: String::new(),
            limit: Some(50),
        },
    )
    .await
    .expect("send scope query");
    let symbols = match recv(&mut client).await.expect("recv scope") {
        ServerMessage::WorkspaceSymbolsResult { symbols, .. } => symbols,
        other => panic!("expected WorkspaceSymbolsResult, got {other:?}"),
    };
    assert!(
        symbols.iter().any(|s| s.display_name == "HandlerFactory"),
        "scope must include HandlerFactory; got {:?}",
        symbols.iter().map(|s| &s.display_name).collect::<Vec<_>>(),
    );
    assert!(
        !symbols.iter().any(|s| s.display_name == "Handler"),
        "scope must exclude `Handler` (wrong scope); got {:?}",
        symbols.iter().map(|s| &s.display_name).collect::<Vec<_>>(),
    );
    assert!(
        !symbols.iter().any(|s| s.display_name == "handle"),
        "scope must exclude `handle` (wrong scope); got {:?}",
        symbols.iter().map(|s| &s.display_name).collect::<Vec<_>>(),
    );

    // (4) modifier_filter — only symbols with `async` modifier.
    send(
        &mut client,
        &ClientMessage::QueryWorkspaceSymbols {
            kind_filter: None,
            scope: None,
            modifier_filter: Some(vec!["async".to_owned()]),
            query: "handle".to_owned(),
            limit: Some(20),
        },
    )
    .await
    .expect("send modifier query");
    let symbols = match recv(&mut client).await.expect("recv modifier") {
        ServerMessage::WorkspaceSymbolsResult { symbols, .. } => symbols,
        other => panic!("expected WorkspaceSymbolsResult, got {other:?}"),
    };
    assert!(
        !symbols.is_empty(),
        "expected at least one async symbol matching `handle`"
    );
    assert!(
        symbols
            .iter()
            .all(|s| s.modifiers.iter().any(|m| m == "async")),
        "modifier_filter violated; modifiers: {:?}",
        symbols.iter().map(|s| &s.modifiers).collect::<Vec<_>>(),
    );

    // (5) empty query → `ranked` is empty (legacy behavior preserved).
    send(
        &mut client,
        &ClientMessage::QueryWorkspaceSymbols {
            kind_filter: None,
            scope: None,
            modifier_filter: None,
            query: String::new(),
            limit: Some(10),
        },
    )
    .await
    .expect("send empty query");
    let ranked = match recv(&mut client).await.expect("recv empty") {
        ServerMessage::WorkspaceSymbolsResult { ranked, .. } => ranked,
        other => panic!("expected WorkspaceSymbolsResult, got {other:?}"),
    };
    assert!(
        ranked.is_empty(),
        "empty query must produce empty ranked (legacy behavior); got {} entries",
        ranked.len(),
    );

    task.abort();
    let _ = task.await;
}
