use anyhow::Result;
use clap::{Subcommand, ValueEnum};

use crate::config::RouteKey;
use crate::types::BackendId;

mod backend;
mod ensemble;
mod init;
mod route;
mod show;

#[derive(Subcommand, Debug)]
pub enum Action {
    /// Show the current effective config.
    Show,
    /// Write the default config to ~/.config/agentpit/config.toml.
    Init {
        /// Overwrite if it already exists.
        #[arg(long)]
        force: bool,
    },
    /// Set a backend's transport (exec/acp). Launches an interactive picker.
    Backend {
        /// Backend to configure.
        id: BackendId,
    },
    /// Set the default backend for a tool.
    Route {
        /// Tool name: rescue / review / explain / refactor.
        tool: RouteKey,
        /// Backend to use. Omit for an interactive picker.
        #[arg(long)]
        backend: Option<BackendId>,
    },
    /// Edit ensemble members + aggregator.
    Ensemble {
        /// Target: default (used by `agentpit ensemble`) or review (used by `agentpit review`).
        target: EnsembleTarget,
    },
}

#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
#[clap(rename_all = "lowercase")]
pub enum EnsembleTarget {
    Default,
    Review,
    Rescue,
    Refactor,
}

impl EnsembleTarget {
    pub fn as_str(&self) -> &'static str {
        match self {
            EnsembleTarget::Default => "default",
            EnsembleTarget::Review => "review",
            EnsembleTarget::Rescue => "rescue",
            EnsembleTarget::Refactor => "refactor",
        }
    }
}

pub async fn run(action: Action) -> Result<()> {
    match action {
        Action::Show => show::run().await,
        Action::Init { force } => init::run(force).await,
        Action::Backend { id } => backend::run(id).await,
        Action::Route { tool, backend } => route::run(tool, backend).await,
        Action::Ensemble { target } => ensemble::run(target).await,
    }
}
