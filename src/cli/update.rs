use anyhow::Result;
use tokio::task;

use crate::update;

pub async fn run(check_only: bool) -> Result<()> {
    if check_only {
        let cache = task::spawn_blocking(update::refresh_cache).await??;
        let current = update::current_version();
        let latest = cache.latest_tag.trim_start_matches('v');
        if update::version_is_newer(&cache.latest_tag, current) {
            println!("update available: {current} -> {latest} (run `agentpit update`)");
        } else {
            println!("agentpit {current} is up to date (latest: {latest}).");
        }
        return Ok(());
    }

    let outcome = task::spawn_blocking(update::perform_update).await??;
    if outcome.already_up_to_date {
        println!(
            "agentpit {} is already up to date.",
            outcome.installed_version
        );
    } else {
        println!("agentpit updated to {}.", outcome.installed_version);
    }
    Ok(())
}
