use anyhow::{Result, anyhow};
use console::style;

use super::config::{Action as ConfigAction, EnsembleTarget};
use crate::config::RouteKey;
use crate::types::BackendId;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MainAction {
    Rescue,
    Review,
    SecurityReview,
    AdversarialReview,
    Explain,
    Refactor,
    Ensemble,
    Config,
    Status,
    Login,
    Update,
    Init,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigMenu {
    Show,
    Init,
    Backend,
    Route,
    Ensemble,
}

pub async fn run_main() -> Result<()> {
    cliclack::intro(style(" agentpit ").on_cyan().black())
        .map_err(|e| anyhow!("intro: {e}"))?;

    let action = cliclack::select("What do you want to do?")
        .item(MainAction::Rescue, "rescue", "one-shot task to a backend")
        .item(MainAction::Review, "review", "multi-agent code review")
        .item(MainAction::SecurityReview, "security-review", "OWASP-style multi-agent security review")
        .item(MainAction::AdversarialReview, "adversarial-review", "challenge assumptions; demand evidence")
        .item(MainAction::Explain, "explain", "explain a target")
        .item(MainAction::Refactor, "refactor", "plan a refactor")
        .item(MainAction::Ensemble, "ensemble", "fan a prompt out to multiple backends")
        .item(MainAction::Config, "config", "modify config (backends / routes / ensemble)")
        .item(MainAction::Status, "status", "show transport + auth state")
        .item(MainAction::Login, "login", "authenticate a backend")
        .item(MainAction::Update, "update", "check for or install a newer release")
        .item(MainAction::Init, "init", "install commands + skills into .claude/")
        .interact()
        .map_err(|e| anyhow!("select: {e}"))?;

    match action {
        MainAction::Rescue => rescue_flow().await,
        MainAction::Review => review_flow().await,
        MainAction::SecurityReview => security_review_flow().await,
        MainAction::AdversarialReview => adversarial_review_flow().await,
        MainAction::Explain => explain_flow().await,
        MainAction::Refactor => refactor_flow().await,
        MainAction::Ensemble => ensemble_flow().await,
        MainAction::Config => run_config().await,
        MainAction::Status => super::status::run(None).await,
        MainAction::Login => login_flow().await,
        MainAction::Update => update_flow().await,
        MainAction::Init => super::init::run(None, false, false).await,
    }
}

pub async fn run_config() -> Result<()> {
    cliclack::intro(style(" agentpit config ").on_cyan().black())
        .map_err(|e| anyhow!("intro: {e}"))?;

    let action = cliclack::select("Config action")
        .item(ConfigMenu::Show, "show", "print current config")
        .item(ConfigMenu::Init, "init", "write defaults to disk")
        .item(ConfigMenu::Backend, "backend", "set a backend's transport (exec/acp)")
        .item(ConfigMenu::Route, "route", "set default backend for a tool")
        .item(
            ConfigMenu::Ensemble,
            "ensemble",
            "edit ensemble members + aggregator",
        )
        .interact()
        .map_err(|e| anyhow!("select: {e}"))?;

    match action {
        ConfigMenu::Show => super::config::run(ConfigAction::Show).await,
        ConfigMenu::Init => {
            let force = cliclack::confirm("Overwrite if config already exists?")
                .initial_value(false)
                .interact()
                .map_err(|e| anyhow!("confirm: {e}"))?;
            super::config::run(ConfigAction::Init { force }).await
        }
        ConfigMenu::Backend => {
            let id = pick_backend("Backend to configure")?;
            super::config::run(ConfigAction::Backend { id }).await
        }
        ConfigMenu::Route => {
            let tool = pick_route_key("Tool to set")?;
            super::config::run(ConfigAction::Route {
                tool,
                backend: None,
            })
            .await
        }
        ConfigMenu::Ensemble => {
            let target = pick_ensemble_target("Target")?;
            super::config::run(ConfigAction::Ensemble { target }).await
        }
    }
}

async fn rescue_flow() -> Result<()> {
    let task: String = cliclack::input("Task to rescue")
        .placeholder("e.g. \"list files in src/ and explain main.rs\"")
        .interact()
        .map_err(|e| anyhow!("input: {e}"))?;
    super::rescue::run(task, None, None, true).await
}

async fn review_flow() -> Result<()> {
    let target: String = cliclack::input("Target to review")
        .placeholder("file path / diff / 'last commit'")
        .interact()
        .map_err(|e| anyhow!("input: {e}"))?;
    let focus_raw: String = cliclack::input("Optional reviewer focus (leave blank for default)")
        .default_input("")
        .required(false)
        .interact()
        .map_err(|e| anyhow!("input: {e}"))?;
    let focus = if focus_raw.trim().is_empty() {
        None
    } else {
        Some(focus_raw)
    };
    super::review::run(target, focus, None, None, None).await
}

async fn security_review_flow() -> Result<()> {
    let target: String = cliclack::input("Target to security-review")
        .placeholder("file path / diff / 'last commit'")
        .interact()
        .map_err(|e| anyhow!("input: {e}"))?;
    let focus_raw: String = cliclack::input("Optional focus area (leave blank for full checklist)")
        .default_input("")
        .required(false)
        .interact()
        .map_err(|e| anyhow!("input: {e}"))?;
    let focus = if focus_raw.trim().is_empty() {
        None
    } else {
        Some(focus_raw)
    };
    super::security_review::run(target, focus, None, None, None).await
}

async fn adversarial_review_flow() -> Result<()> {
    let target: String = cliclack::input("Target to adversarial-review")
        .placeholder("file path / diff / 'last commit'")
        .interact()
        .map_err(|e| anyhow!("input: {e}"))?;
    let focus_raw: String = cliclack::input("Optional attack focus (leave blank for full checklist)")
        .default_input("")
        .required(false)
        .interact()
        .map_err(|e| anyhow!("input: {e}"))?;
    let focus = if focus_raw.trim().is_empty() {
        None
    } else {
        Some(focus_raw)
    };
    super::adversarial_review::run(target, focus, None, None, None).await
}

async fn explain_flow() -> Result<()> {
    let target: String = cliclack::input("Target to explain")
        .placeholder("file path / symbol")
        .interact()
        .map_err(|e| anyhow!("input: {e}"))?;
    let deep = cliclack::confirm("Deep explanation?")
        .initial_value(false)
        .interact()
        .map_err(|e| anyhow!("confirm: {e}"))?;
    super::explain::run(target, deep, None, None).await
}

async fn refactor_flow() -> Result<()> {
    let path: String = cliclack::input("File or path to refactor")
        .interact()
        .map_err(|e| anyhow!("input: {e}"))?;
    let goal: String = cliclack::input("Refactor goal")
        .placeholder("e.g. \"extract X into its own module\"")
        .interact()
        .map_err(|e| anyhow!("input: {e}"))?;
    super::refactor::run(path, goal, None, None).await
}

async fn ensemble_flow() -> Result<()> {
    let prompt: String = cliclack::input("Prompt to fan out")
        .interact()
        .map_err(|e| anyhow!("input: {e}"))?;
    super::ensemble::run(prompt, None, None, None).await
}

async fn login_flow() -> Result<()> {
    let backend = pick_backend("Backend to authenticate")?;
    let check_only = cliclack::confirm("Check only (do not launch login)?")
        .initial_value(false)
        .interact()
        .map_err(|e| anyhow!("confirm: {e}"))?;
    super::login::run(backend, check_only).await
}

async fn update_flow() -> Result<()> {
    let check_only = cliclack::confirm("Check only (do not download)?")
        .initial_value(true)
        .interact()
        .map_err(|e| anyhow!("confirm: {e}"))?;
    super::update::run(check_only).await
}

fn pick_backend(prompt: &str) -> Result<BackendId> {
    cliclack::select(prompt)
        .item(BackendId::Antigravity, "antigravity", "agy — Gemini CLI successor")
        .item(BackendId::Gemini, "gemini", "")
        .item(BackendId::Claude, "claude", "")
        .item(BackendId::Codex, "codex", "")
        .item(BackendId::Opencode, "opencode", "")
        .interact()
        .map_err(|e| anyhow!("select: {e}"))
}

fn pick_route_key(prompt: &str) -> Result<RouteKey> {
    cliclack::select(prompt)
        .item(RouteKey::Rescue, "rescue", "")
        .item(RouteKey::Review, "review", "")
        .item(RouteKey::Explain, "explain", "")
        .item(RouteKey::Refactor, "refactor", "")
        .interact()
        .map_err(|e| anyhow!("select: {e}"))
}

fn pick_ensemble_target(prompt: &str) -> Result<EnsembleTarget> {
    cliclack::select(prompt)
        .item(EnsembleTarget::Default, "default", "agentpit ensemble subcommand")
        .item(EnsembleTarget::Review, "review", "agentpit review")
        .item(EnsembleTarget::SecurityReview, "security-review", "agentpit security-review")
        .item(EnsembleTarget::Rescue, "rescue", "agentpit rescue")
        .item(EnsembleTarget::Refactor, "refactor", "agentpit refactor")
        .interact()
        .map_err(|e| anyhow!("select: {e}"))
}
