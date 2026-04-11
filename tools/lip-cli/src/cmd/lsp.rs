use std::path::PathBuf;

use clap::Args;
use tower_lsp::{LspService, Server};

use lip::bridge::lsp_server::LipLspBackend;

#[derive(Args)]
pub struct LspArgs {
    /// Path to the LIP daemon Unix socket.
    #[arg(long, default_value = "/tmp/lip-daemon.sock")]
    pub socket: PathBuf,
}

pub async fn run(args: LspArgs) -> anyhow::Result<()> {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, messages) =
        LspService::build(|client| LipLspBackend::new(client, args.socket)).finish();

    Server::new(stdin, stdout, messages).serve(service).await;
    Ok(())
}
