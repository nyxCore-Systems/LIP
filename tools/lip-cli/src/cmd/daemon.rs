use std::path::PathBuf;

use clap::Args;
use lip::daemon::LipDaemon;

#[derive(Args)]
pub struct DaemonArgs {
    /// Path for the Unix domain socket the daemon will listen on.
    #[arg(long, default_value = "/tmp/lip-daemon.sock")]
    pub socket: PathBuf,
}

pub async fn run(args: DaemonArgs) -> anyhow::Result<()> {
    LipDaemon::new(&args.socket).run().await
}
