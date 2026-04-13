use std::path::PathBuf;

use clap::Args;
use lip::daemon::LipDaemon;

#[derive(Args)]
pub struct DaemonArgs {
    /// Path for the Unix domain socket the daemon will listen on.
    #[arg(long, default_value = "/tmp/lip-daemon.sock")]
    pub socket: PathBuf,

    /// Monitor the parent process and exit automatically when it terminates.
    /// Intended for IDE integrations that spawn the daemon as a managed subprocess.
    #[arg(long, default_value_t = false)]
    pub managed: bool,
}

pub async fn run(args: DaemonArgs) -> anyhow::Result<()> {
    LipDaemon::new(&args.socket)
        .managed(args.managed)
        .run()
        .await
}
