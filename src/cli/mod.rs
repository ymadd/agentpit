use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::types::BackendId;

mod common;
pub mod adversarial_review;
pub mod config;
pub mod dashboard;
pub mod ensemble;
pub mod explain;
pub mod init;
pub mod login;
mod menu;
pub mod refactor;
pub mod rescue;
pub mod review;
pub mod security_review;
pub mod status;
pub mod update;

pub(crate) use common::{
    install_ctrlc_cancel, load_context, resolve_cwd, stdout_streamer,
};

#[derive(Parser, Debug)]
#[command(
    name = "agentpit",
    version,
    about = "Multi-agent hub CLI: route work to Gemini / Antigravity (agy) / Claude / Codex / OpenCode."
)]
pub struct Cli {
    /// Run a subcommand. Omit to launch the interactive menu.
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Delegate a one-shot task to a backend.
    Rescue {
        /// Task description (positional). Quote multi-word tasks.
        task: String,
        /// Override target backend.
        #[arg(long)]
        backend: Option<BackendId>,
        /// Working directory (defaults to current).
        #[arg(long)]
        cwd: Option<String>,
        /// Disable auto-login on auth failure.
        #[arg(long, default_value_t = false)]
        no_auto_login: bool,
    },

    /// Run a multi-agent code review (defaults to antigravity + opencode).
    Review {
        target: String,
        #[arg(long)]
        focus: Option<String>,
        #[arg(long, value_delimiter = ',')]
        members: Option<Vec<BackendId>>,
        #[arg(long)]
        aggregator: Option<BackendId>,
        #[arg(long)]
        cwd: Option<String>,
    },

    /// Run a multi-agent security review (OWASP-style checklist, defaults to claude + codex).
    #[command(name = "security-review")]
    SecurityReview {
        target: String,
        #[arg(long)]
        focus: Option<String>,
        #[arg(long, value_delimiter = ',')]
        members: Option<Vec<BackendId>>,
        #[arg(long)]
        aggregator: Option<BackendId>,
        #[arg(long)]
        cwd: Option<String>,
    },

    /// Run a multi-agent adversarial review (challenges assumptions, demands evidence; defaults to codex + antigravity).
    #[command(name = "adversarial-review")]
    AdversarialReview {
        target: String,
        #[arg(long)]
        focus: Option<String>,
        #[arg(long, value_delimiter = ',')]
        members: Option<Vec<BackendId>>,
        #[arg(long)]
        aggregator: Option<BackendId>,
        #[arg(long)]
        cwd: Option<String>,
    },

    /// Explain a target via a backend agent.
    Explain {
        target: String,
        #[arg(long)]
        deep: bool,
        #[arg(long)]
        backend: Option<BackendId>,
        #[arg(long)]
        cwd: Option<String>,
    },

    /// Plan a refactor via a backend agent.
    Refactor {
        path: String,
        goal: String,
        #[arg(long)]
        backend: Option<BackendId>,
        #[arg(long)]
        cwd: Option<String>,
    },

    /// Fan a prompt out to multiple backends in parallel.
    Ensemble {
        prompt: String,
        #[arg(long, value_delimiter = ',')]
        members: Option<Vec<BackendId>>,
        #[arg(long)]
        aggregator: Option<BackendId>,
        #[arg(long)]
        cwd: Option<String>,
    },

    /// Launch the live desktop dashboard (separate app).
    Dashboard,

    /// Show config + backend transport + auth state.
    Status {
        #[arg(long)]
        backend: Option<BackendId>,
    },

    /// Check or launch a backend's login flow.
    Login {
        backend: BackendId,
        #[arg(long)]
        check: bool,
    },

    /// Install slash commands and skills into .claude/ (interactive picker if --scope is omitted).
    Init {
        /// Install scope. Omit for an interactive picker.
        #[arg(long, value_enum)]
        scope: Option<init::Scope>,
        /// Overwrite existing files.
        #[arg(long)]
        force: bool,
        /// Refresh existing installs in every detected scope (no prompt). Used internally by `agentpit update`.
        #[arg(long)]
        refresh: bool,
    },

    /// Check for or install a newer agentpit release from GitHub.
    Update {
        /// Only check; do not download or replace the binary.
        #[arg(long)]
        check: bool,
    },

    /// Inspect or modify the config file (~/.config/agentpit/config.toml). Omit the
    /// sub-action to launch an interactive menu.
    Config {
        #[command(subcommand)]
        action: Option<config::Action>,
    },
}

pub async fn run(cli: Cli) -> Result<()> {
    let command = match cli.command {
        Some(c) => c,
        None => return menu::run_main().await,
    };
    match command {
        Command::Rescue {
            task,
            backend,
            cwd,
            no_auto_login,
        } => rescue::run(task, backend, cwd, !no_auto_login).await,

        Command::Review {
            target,
            focus,
            members,
            aggregator,
            cwd,
        } => review::run(target, focus, members, aggregator, cwd).await,

        Command::SecurityReview {
            target,
            focus,
            members,
            aggregator,
            cwd,
        } => security_review::run(target, focus, members, aggregator, cwd).await,

        Command::AdversarialReview {
            target,
            focus,
            members,
            aggregator,
            cwd,
        } => adversarial_review::run(target, focus, members, aggregator, cwd).await,

        Command::Explain {
            target,
            deep,
            backend,
            cwd,
        } => explain::run(target, deep, backend, cwd).await,

        Command::Refactor {
            path,
            goal,
            backend,
            cwd,
        } => refactor::run(path, goal, backend, cwd).await,

        Command::Ensemble {
            prompt,
            members,
            aggregator,
            cwd,
        } => ensemble::run(prompt, members, aggregator, cwd).await,

        Command::Dashboard => dashboard::run().await,

        Command::Status { backend } => status::run(backend).await,

        Command::Login { backend, check } => login::run(backend, check).await,

        Command::Init {
            scope,
            force,
            refresh,
        } => init::run(scope, force, refresh).await,

        Command::Update { check } => update::run(check).await,

        Command::Config { action } => match action {
            Some(a) => config::run(a).await,
            None => menu::run_config().await,
        },
    }
}
