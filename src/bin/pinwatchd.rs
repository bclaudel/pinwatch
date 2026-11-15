use anyhow::Result;
use clap::Parser;
use pinwatch::{
    config::{Config, Mode},
    daemon,
};
use std::path::PathBuf;
#[derive(Parser)]
#[command(
    version,
    about = "GPU connector polling daemon; monitor-only unless explicitly configured active"
)]
struct Args {
    #[arg(long)]
    config: Option<PathBuf>,
    /// Force read-only monitoring, overriding the configuration.
    #[arg(long, conflicts_with = "dry_run")]
    monitor_only: bool,
    /// Propose interventions but never change GPU controls.
    #[arg(long)]
    dry_run: bool,
}
#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let mut config = if let Some(path) = args.config {
        Config::read(&path)?
    } else {
        Config::default()
    };
    if args.monitor_only {
        config.mode = Mode::Monitor;
    }
    if args.dry_run {
        config.mode = Mode::DryRun;
    }
    daemon::run(config).await
}
