//! `agentpit similarity init | status` — manage the kNN routing layer's embedding model
//! (`--features similarity` builds only).

use anyhow::Result;
use clap::Subcommand;

use crate::similarity::{embed, parse_samples, routes_path};

#[derive(Subcommand, Debug)]
pub enum Action {
    /// Download the embedding model (multilingual-e5-small, ~120MB) into the state dir and
    /// enable similarity routing.
    Init,
    /// Show whether the model is installed and how many routing samples exist.
    Status,
}

/// `/similarity …` on an interactive surface, parsed with this subcommand's own clap
/// grammar (see `arena::run_words` for why the slash surfaces reuse clap).
#[derive(clap::Parser, Debug)]
#[command(name = "/similarity", no_binary_name = true)]
struct Words {
    #[command(subcommand)]
    action: Action,
}

/// Run `/similarity <words>`.
pub async fn run_words(words: Vec<String>) -> Result<()> {
    match <Words as clap::Parser>::try_parse_from(words) {
        Ok(parsed) => run(parsed.action).await,
        Err(e) => {
            let _ = e.print();
            Ok(())
        }
    }
}

pub async fn run(action: Action) -> Result<()> {
    match action {
        Action::Init => {
            if embed::model_ready() {
                println!("embedding model already installed.");
                return Ok(());
            }
            println!("downloading multilingual-e5-small (one-time, ~120MB)…");
            tokio::task::spawn_blocking(embed::init).await??;
            println!("similarity routing is ready. Samples accrue via `agentpit profile learn`.");
            Ok(())
        }
        Action::Status => {
            let ready = embed::model_ready();
            let samples = std::fs::read_to_string(routes_path())
                .map(|raw| parse_samples(&raw).len())
                .unwrap_or(0);
            println!(
                "model: {}\nsamples: {samples} ({})",
                if ready {
                    "installed"
                } else {
                    "not installed (run `agentpit similarity init`)"
                },
                routes_path().display()
            );
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn slash_words_carry_the_sub_action_with_no_binary_name_in_front() {
        assert!(matches!(
            Words::try_parse_from(["status"]).unwrap().action,
            Action::Status
        ));
        // The sub-action is required: `/similarity` alone is a usage error, not a default.
        assert!(Words::try_parse_from(Vec::<String>::new()).is_err());
    }
}
