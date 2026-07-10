use anyhow::Result;
use clap::{Parser, Subcommand};

use crate::types::BackendId;

pub mod adversarial_review;
pub mod ask;
pub(crate) mod cancel;
mod common;
pub mod config;
pub mod dashboard;
pub mod diagnose;
pub mod ensemble;
pub mod explain;
pub mod init;
pub mod login;
pub mod mcp_cmd;
mod menu;
pub mod note;
pub mod profile;
pub mod refactor;
pub mod refute;
pub mod refute_bench;
pub mod repl;
pub mod rescue;
pub mod review;
pub mod security_review;
pub mod status;
pub mod update;
pub mod workflow;

pub(crate) use common::{install_ctrlc_cancel, load_context, resolve_cwd, stdout_streamer};

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
        /// Dispatch to a configured `[workflow.roles.<name>]` persona instead of an explicit
        /// backend. Mutually exclusive with --backend (role dispatch is always single-backend:
        /// the role itself resolves which backend plays it).
        #[arg(long)]
        role: Option<String>,
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

    /// Run a model-driven workflow: a manager backend (claude|codex) orchestrates sub-agents.
    Workflow {
        /// High-level goal for the manager to decompose and orchestrate.
        goal: String,
        /// Manager backend (claude|codex). Defaults to [workflow].manager_backend or default.backend.
        #[arg(long)]
        manager: Option<BackendId>,
        /// Worker backends the manager may dispatch to (comma-separated). Defaults to all available minus the manager.
        #[arg(long, value_delimiter = ',')]
        agents: Option<Vec<BackendId>>,
        /// Hard recursion depth ceiling.
        #[arg(long)]
        max_depth: Option<u32>,
        /// Orchestrate via the MCP channel (claude manager only) instead of shelling out.
        #[arg(long, default_value_t = false)]
        use_mcp: bool,
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

    /// Inspect or (re)seed the capability profiles in profiles.toml. Omit the sub-action to
    /// show the matrix.
    Profile {
        #[command(subcommand)]
        action: Option<profile::Action>,
    },

    /// Dry-run task diagnosis + profile routing (features → category → backend). `--json`
    /// emits a machine-readable verdict for downstream automation.
    Diagnose {
        /// The task to diagnose. Quote multi-word tasks.
        task: String,
        /// Emit machine-readable JSON instead of the human-readable summary.
        #[arg(long)]
        json: bool,
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

    /// MCP channel: run a stdio MCP server exposing agentpit's dispatch/ensemble tools.
    Mcp {
        #[command(subcommand)]
        action: mcp_cmd::Action,
    },

    /// Launch the persistent conversational REPL (the default when no subcommand is given).
    Repl,

    /// Ask the supervising human a question and block for an answer (the CLI back-channel a
    /// shell-out workflow manager uses; prints the answer or HUMAN_UNAVAILABLE to stdout).
    Ask {
        /// The question to put to the human.
        prompt: String,
        /// Explicit options the human picks from. Repeat the flag per option.
        #[arg(long = "option")]
        options: Vec<String>,
        /// "blocking" (a worker is stalled) or "review" (nothing blocked). Defaults to review.
        #[arg(long)]
        kind: Option<String>,
        /// Seconds to wait before returning HUMAN_UNAVAILABLE (default 180, capped 600).
        #[arg(long)]
        timeout_secs: Option<u64>,
    },

    /// Append a durable conversation-layer note to the run transcript: a 1→1 handoff or a shared
    /// board entry (the CLI twin of the MCP `post_note` tool). Manager-only; a worker is a
    /// silent no-op.
    Note {
        /// The note body — the context being handed off, or the board entry.
        body: String,
        /// "handoff" (1→1 context pass, default) or "board" (shared scratch entry).
        #[arg(long)]
        kind: Option<String>,
        /// The backend that authored this note (e.g. the handed-off worker). Omit for a manager post.
        #[arg(long)]
        from: Option<BackendId>,
    },

    /// Run a refutation (④): dispatch an adversarial critic at a stuck candidate, then a defender
    /// carrying that critique, and print both for the manager to adjudicate. One depth-guarded
    /// pass — not a loop.
    Refute {
        /// The stuck candidate to put under adversarial scrutiny.
        candidate: String,
        /// The sub-task the candidate was meant to achieve (gives the critic/defender their target).
        #[arg(long)]
        task: String,
        /// Backend that produces the critique. Defaults to the adversarial-review primary.
        #[arg(long)]
        critic: Option<BackendId>,
        /// Backend that produces the defense. Defaults to a backend distinct from the critic.
        #[arg(long)]
        defender: Option<BackendId>,
        #[arg(long)]
        cwd: Option<String>,
    },

    /// Gate ④ refute itself (design §5.1): run the live critique→defense legs against a small set
    /// of deliberately-broken "stuck" candidates and check whether the defense's revision actually
    /// scores better than the stuck candidate did, not just that it produced *something*. Green
    /// only when every probe clears the pass margin.
    RefuteBench {
        /// Backend that produces the critique. Defaults to the adversarial-review primary.
        #[arg(long)]
        critic: Option<BackendId>,
        /// Backend that produces the defense. Defaults to a backend distinct from the critic.
        #[arg(long)]
        defender: Option<BackendId>,
        #[arg(long)]
        cwd: Option<String>,
    },
}

pub async fn run(cli: Cli) -> Result<()> {
    let command = match cli.command {
        Some(c) => c,
        None => return repl::run_repl().await,
    };
    match command {
        Command::Rescue {
            task,
            role,
            backend,
            cwd,
            no_auto_login,
        } => rescue::run_with_role(task, role, backend, cwd, !no_auto_login).await,

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

        Command::Workflow {
            goal,
            manager,
            agents,
            max_depth,
            use_mcp,
            cwd,
        } => workflow::run(goal, manager, agents, max_depth, use_mcp, cwd).await,

        Command::Dashboard => dashboard::run().await,

        Command::Status { backend } => status::run(backend).await,

        Command::Profile { action } => profile::run(action).await,

        Command::Diagnose { task, json } => diagnose::run(task, json).await,

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

        Command::Mcp { action } => mcp_cmd::run(action).await,

        Command::Repl => repl::run_repl().await,

        Command::Ask {
            prompt,
            options,
            kind,
            timeout_secs,
        } => ask::run(prompt, options, kind, timeout_secs).await,

        Command::Note { body, kind, from } => note::run(body, kind, from).await,

        Command::Refute {
            candidate,
            task,
            critic,
            defender,
            cwd,
        } => refute::run(candidate, task, critic, defender, cwd).await,

        Command::RefuteBench {
            critic,
            defender,
            cwd,
        } => refute_bench::run(critic, defender, cwd).await,
    }
}
