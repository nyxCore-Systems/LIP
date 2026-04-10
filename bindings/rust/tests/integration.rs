/// End-to-end integration test: daemon ↔ client over a real Unix socket.
///
/// Tests the full pipeline:
///   1. ManifestRequest  → ManifestResponse  (handshake)
///   2. Delta (Upsert)   → DeltaStream       (file index + ack)
///   3. QueryDefinition  → DefinitionResult  (query after index)
///   4. Delta (Delete)   → DeltaStream       (removal)
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

use lip::daemon::LipDaemon;
use lip::query_graph::{ClientMessage, ServerMessage};
use lip::schema::{Action, IndexingState, OwnedDocument};

// ─── Framing helpers (client side) ───────────────────────────────────────────

async fn send(stream: &mut UnixStream, msg: &ClientMessage) -> anyhow::Result<()> {
    let body = serde_json::to_vec(msg)?;
    stream
        .write_all(&(body.len() as u32).to_be_bytes())
        .await?;
    stream.write_all(&body).await?;
    Ok(())
}

async fn recv(stream: &mut UnixStream) -> anyhow::Result<ServerMessage> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    let mut body = vec![0u8; len];
    stream.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

// ─── Helper: build a document with known source ───────────────────────────────

fn make_doc(uri: &str, source: &str) -> OwnedDocument {
    OwnedDocument {
        uri:          uri.to_owned(),
        content_hash: lip::schema::sha256_hex(source.as_bytes()),
        language:     "rust".to_owned(),
        occurrences:  vec![],
        symbols:      vec![],
        merkle_path:  uri.to_owned(),
        edges:        vec![],
        source_text:  Some(source.to_owned()),
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
        &ClientMessage::Manifest(lip::daemon::ManifestRequest {
            repo_root:     "/tmp/test-repo".to_owned(),
            merkle_root:   "abc123".to_owned(),
            dep_tree_hash: "def456".to_owned(),
            lip_version:   "0.1.0".to_owned(),
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
            seq:      42,
            action:   Action::Upsert,
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
            uri:  uri.to_owned(),
            line: 0,
            col:  0,
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
            seq:      43,
            action:   Action::Delete,
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
    let mut client = UnixStream::connect(&socket_path)
        .await
        .expect("connect");

    // Index two files.
    for i in 0..2 {
        let source = format!("pub struct Widget{i}; pub fn make_{i}() -> Widget{i} {{ Widget{i} }}");
        let uri = format!("lip://local/test@0.1/w{i}.rs");
        send(
            &mut client,
            &ClientMessage::Delta {
                seq:      i as u64,
                action:   Action::Upsert,
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
