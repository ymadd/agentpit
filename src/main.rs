use std::time::Duration;

use anyhow::Result;
use clap::Parser;

use agentpit::cli::{Cli, Command, run};
use agentpit::update;

async fn maybe_show_banner(cli: &Cli) {
    if matches!(cli.command, Some(Command::Update { .. })) {
        return;
    }
    let task = tokio::task::spawn_blocking(update::compute_banner);
    if let Ok(Ok(Some(banner))) = tokio::time::timeout(Duration::from_secs(2), task).await {
        eprintln!("{banner}");
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    maybe_show_banner(&cli).await;
    run(cli).await
}
