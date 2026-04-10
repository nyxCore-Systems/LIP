//! LIP Registry Server
//!
//! Serves pre-built dependency slices (`OwnedDependencySlice` JSON) over HTTP.
//! Clients (the `lip fetch` command and the daemon's registry client) fetch slices
//! by SHA-256 content hash.
//!
//! ## Endpoints
//!
//! | Method | Path                | Description                                  |
//! |--------|---------------------|----------------------------------------------|
//! | GET    | `/slices/{hash}`    | Fetch a slice by content hash                |
//! | PUT    | `/slices/{hash}`    | Publish a new slice (hash must match body)   |
//! | GET    | `/health`           | Returns `{"ok":true}`                        |
//!
//! ## Usage
//!
//! ```text
//! lip-registry serve --store ./slices --port 8080
//! ```

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use axum::body::Bytes;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::get;
use axum::Router;
use clap::{Parser, Subcommand};
use serde_json::json;
use tracing::{info, warn};

use lip::schema::{sha256_hex, OwnedDependencySlice};

// ─── CLI ─────────────────────────────────────────────────────────────────────

#[derive(Parser)]
#[command(name = "lip-registry", about = "LIP slice registry server")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the HTTP server.
    Serve(ServeArgs),
}

#[derive(Parser)]
struct ServeArgs {
    /// Directory where slice JSON files are stored.
    #[arg(long, default_value = "./slices")]
    store: PathBuf,

    /// TCP port to listen on.
    #[arg(long, default_value_t = 8080)]
    port: u16,

    /// Bind address.
    #[arg(long, default_value = "0.0.0.0")]
    host: String,
}

// ─── App state ───────────────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    store: Arc<PathBuf>,
}

impl AppState {
    fn slice_path(&self, hash: &str) -> PathBuf {
        self.store.join(format!("{hash}.slice.json"))
    }
}

// ─── Handlers ────────────────────────────────────────────────────────────────

/// `GET /slices/{hash}` — fetch a slice by its SHA-256 content hash.
async fn get_slice(
    AxumPath(hash): AxumPath<String>,
    State(state): State<AppState>,
) -> Response {
    if !is_valid_hash(&hash) {
        return (StatusCode::BAD_REQUEST, "invalid hash format").into_response();
    }

    let path = state.slice_path(&hash);
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            info!("served slice {hash} ({} bytes)", bytes.len());
            (
                StatusCode::OK,
                [("content-type", "application/json")],
                bytes,
            )
                .into_response()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            (StatusCode::NOT_FOUND, format!("slice {hash} not found")).into_response()
        }
        Err(e) => {
            warn!("error reading slice {hash}: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "storage error").into_response()
        }
    }
}

/// `PUT /slices/{hash}` — publish a new slice.
///
/// The request body must be valid `OwnedDependencySlice` JSON. The SHA-256 of
/// the body must match both the URL `{hash}` parameter and the
/// `content_hash` field inside the document.
async fn put_slice(
    AxumPath(hash): AxumPath<String>,
    State(state): State<AppState>,
    body: Bytes,
) -> Response {
    if !is_valid_hash(&hash) {
        return (StatusCode::BAD_REQUEST, "invalid hash format").into_response();
    }

    // Verify the body is valid JSON and parses as OwnedDependencySlice.
    let slice: OwnedDependencySlice = match serde_json::from_slice(&body) {
        Ok(s) => s,
        Err(e) => {
            return (StatusCode::BAD_REQUEST, format!("invalid slice JSON: {e}")).into_response();
        }
    };

    // Verify hash matches body.
    let actual_hash = sha256_hex(&body);
    if actual_hash != hash {
        return (
            StatusCode::BAD_REQUEST,
            format!("hash mismatch: URL says {hash}, body hashes to {actual_hash}"),
        )
            .into_response();
    }

    // Verify content_hash field is consistent.
    if slice.content_hash != hash {
        return (
            StatusCode::BAD_REQUEST,
            format!(
                "content_hash field ({}) does not match URL hash ({})",
                slice.content_hash, hash
            ),
        )
            .into_response();
    }

    let path = state.slice_path(&hash);

    // Idempotent: if it already exists, return 200.
    if path.exists() {
        info!("slice {hash} already present — idempotent PUT");
        return (StatusCode::OK, Json(json!({"stored": false, "reason": "already exists"}))).into_response();
    }

    match tokio::fs::write(&path, &body).await {
        Ok(()) => {
            info!(
                "stored slice {}/{} v{} ({} bytes)",
                slice.manager, slice.package_name, slice.version, body.len()
            );
            (StatusCode::CREATED, Json(json!({"stored": true, "hash": hash}))).into_response()
        }
        Err(e) => {
            warn!("failed to write slice {hash}: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "storage error").into_response()
        }
    }
}

/// `GET /health` — liveness probe.
async fn health() -> Json<serde_json::Value> {
    Json(json!({"ok": true}))
}

// ─── Validation ──────────────────────────────────────────────────────────────

/// A valid SHA-256 hash is exactly 64 lowercase hex characters.
fn is_valid_hash(s: &str) -> bool {
    s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit())
}

// ─── Entry point ─────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lip_registry=info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Commands::Serve(args) => serve(args).await,
    }
}

async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(&args.store)
        .await
        .with_context(|| format!("creating store directory {:?}", args.store))?;

    let state = AppState {
        store: Arc::new(args.store.clone()),
    };

    let app = Router::new()
        .route("/slices/:hash", get(get_slice).put(put_slice))
        .route("/health",       get(health))
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", args.host, args.port)
        .parse()
        .context("invalid bind address")?;

    info!("lip-registry listening on http://{addr}  store={}", args.store.display());

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
