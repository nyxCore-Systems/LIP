#![recursion_limit = "512"]

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

mod cmd;
mod output;

#[derive(Parser)]
#[command(
    name    = "lip",
    version = env!("CARGO_PKG_VERSION"),
    about   = "LIP — Linked Incremental Protocol CLI"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Log level filter (e.g. "debug", "info", "warn").
    #[arg(long, global = true, default_value = "warn", env = "LIP_LOG")]
    log: String,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the LIP daemon (Unix socket server).
    Daemon(cmd::daemon::DaemonArgs),
    /// Index a directory with the Tier 1 tree-sitter indexer.
    Index(cmd::index::IndexArgs),
    /// Query a running LIP daemon.
    Query(cmd::query::QueryArgs),
    /// Import an existing SCIP index file.
    Import(cmd::import::ImportArgs),
    /// Export a LIP EventStream as a SCIP index.
    Export(cmd::export::ExportArgs),
    /// Start an LSP bridge that forwards to a LIP daemon.
    Lsp(cmd::lsp::LspArgs),
    /// Fetch a dependency slice from the LIP registry.
    Fetch(cmd::fetch::FetchArgs),
    /// Publish a dependency slice to the LIP registry.
    Push(cmd::push::PushArgs),
    /// Annotate a symbol with a key/value pair.
    Annotate(cmd::annotate::AnnotateArgs),
    /// Start a Model Context Protocol server backed by the LIP daemon.
    Mcp(cmd::mcp::McpArgs),
    /// Build pre-computed dependency slices for Cargo, npm, or pub packages.
    Slice(cmd::slice::SliceArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(&cli.log))
        .with_target(false)
        .init();

    match cli.command {
        Commands::Daemon(args) => cmd::daemon::run(args).await,
        Commands::Index(args) => cmd::index::run(args).await,
        Commands::Query(args) => cmd::query::run(args).await,
        Commands::Import(args) => cmd::import::run(args).await,
        Commands::Export(args) => cmd::export::run(args).await,
        Commands::Lsp(args) => cmd::lsp::run(args).await,
        Commands::Fetch(args) => cmd::fetch::run(args).await,
        Commands::Push(args) => cmd::push::run(args).await,
        Commands::Annotate(args) => cmd::annotate::run(args).await,
        Commands::Mcp(args) => cmd::mcp::run(args).await,
        Commands::Slice(args) => cmd::slice::run(args).await,
    }
}
