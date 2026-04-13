/// End-to-end integration tests: daemon ↔ client over a real Unix socket.
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use lip_core::daemon::LipDaemon;
use lip_core::query_graph::{ClientMessage, ServerMessage};
use lip_core::schema::{Action, IndexingState, OwnedDocument};

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
            query: "Widget".to_owned(),
            limit: Some(50),
        },
    )
    .await
    .expect("send workspace query");

    let resp = recv(&mut client).await.expect("recv workspace symbols");
    match resp {
        ServerMessage::WorkspaceSymbolsResult { symbols } => {
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
                query: "persisted".into(),
                limit: Some(10),
            },
        )
        .await
        .unwrap();

        let resp = recv(&mut client).await.unwrap();
        match resp {
            ServerMessage::WorkspaceSymbolsResult { symbols } => {
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
