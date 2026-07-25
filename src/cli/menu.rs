use console::style;

use super::cancel::{Nav, prompt};
use super::config::{Action as ConfigAction, EnsembleTarget};
use crate::config::{RouteKey, load_config};
use crate::types::BackendId;

// ─── Main-menu action ────────────────────────────────────────────────────────

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
    Quit,
}

// ─── Config-menu action ───────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigMenu {
    Show,
    Init,
    Backend,
    Route,
    Ensemble,
    Back,
}

// ─── run_main ─────────────────────────────────────────────────────────────────

pub async fn run_main() -> anyhow::Result<()> {
    cliclack::intro(style(" agentpit ").on_cyan().black())
        .map_err(|e| anyhow::anyhow!("intro: {e}"))?;

    loop {
        let nav = prompt(
            cliclack::select("What do you want to do?")
                .item(MainAction::Rescue, "rescue", "one-shot task to a backend")
                .item(MainAction::Review, "review", "multi-agent code review")
                .item(
                    MainAction::SecurityReview,
                    "security-review",
                    "OWASP-style multi-agent security review",
                )
                .item(
                    MainAction::AdversarialReview,
                    "adversarial-review",
                    "challenge assumptions; demand evidence",
                )
                .item(MainAction::Explain, "explain", "explain a target")
                .item(MainAction::Refactor, "refactor", "plan a refactor")
                .item(
                    MainAction::Ensemble,
                    "ensemble",
                    "fan a prompt out to multiple backends",
                )
                .item(
                    MainAction::Config,
                    "config",
                    "modify config (backends / routes / ensemble)",
                )
                .item(MainAction::Status, "status", "show transport + auth state")
                .item(MainAction::Login, "login", "authenticate a backend")
                .item(
                    MainAction::Update,
                    "update",
                    "check for or install a newer release",
                )
                .item(
                    MainAction::Init,
                    "init",
                    "install commands + skills into .claude/",
                )
                .item(MainAction::Quit, "quit", "exit agentpit")
                .interact(),
        )?;

        let action = match nav {
            // Esc at the top level = clean quit
            Nav::Back => break,
            Nav::Value(a) => a,
        };

        if action == MainAction::Quit {
            break;
        }

        // Run the selected action. Show errors inline and continue the loop.
        if let Err(e) = dispatch_main(action).await {
            let _ = cliclack::note("error", format!("{e:#}"));
        }
    }

    cliclack::outro("Goodbye!").map_err(|e| anyhow::anyhow!("outro: {e}"))?;
    Ok(())
}

async fn dispatch_main(action: MainAction) -> anyhow::Result<()> {
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
        MainAction::Quit => Ok(()),
    }
}

// ─── run_config ───────────────────────────────────────────────────────────────

pub async fn run_config() -> anyhow::Result<()> {
    // No top-level intro/outro here: each individual config action (backend,
    // route, ensemble) owns its own intro+outro frame, and show/init use plain
    // output.  Adding a frame here would produce a double-intro whenever an
    // action is selected.  The config menu is visually just a sub-menu of the
    // main loop — it needs no extra wrapper.
    loop {
        // Load config on every iteration so the inline labels are always fresh.
        let loaded = load_config(None).ok();

        // Build inline current-value labels for each configurable menu item.
        let backend_hint = loaded.as_ref().map_or_else(
            || "set a backend's transport".into(),
            |l| {
                if l.config.backends.is_empty() {
                    "set a backend's transport (all: default)".into()
                } else {
                    let summary: Vec<String> = l
                        .config
                        .backends
                        .iter()
                        .map(|(id, ov)| {
                            let t = ov
                                .transport
                                .map(|t| t.as_str().to_string())
                                .unwrap_or_else(|| "default".into());
                            format!("{id}={t}")
                        })
                        .collect();
                    format!("set a backend's transport  [{}]", summary.join(", "))
                }
            },
        );

        let route_hint = loaded.as_ref().map_or_else(
            || "set default backend for a tool".into(),
            |l| {
                if l.config.routes.is_empty() {
                    "set default backend for a tool (none set)".into()
                } else {
                    let summary: Vec<String> = l
                        .config
                        .routes
                        .iter()
                        .map(|(k, b)| format!("{k}={b}"))
                        .collect();
                    format!("set default backend for a tool  [{}]", summary.join(", "))
                }
            },
        );

        let ensemble_hint = loaded.as_ref().map_or_else(
            || "edit ensemble members + aggregator".into(),
            |l| {
                let members = &l.config.ensemble.default_members;
                let agg = l.config.ensemble.aggregator;
                let m_str = if members.is_empty() {
                    "(none)".into()
                } else {
                    members
                        .iter()
                        .map(BackendId::to_string)
                        .collect::<Vec<_>>()
                        .join("+")
                };
                let a_str = agg
                    .map(|b| b.to_string())
                    .unwrap_or_else(|| "(none)".into());
                format!("edit ensemble members + aggregator  [members: {m_str}  agg: {a_str}]")
            },
        );

        let nav = prompt(
            cliclack::select("Config action")
                .item(ConfigMenu::Show, "show", "print current config")
                .item(ConfigMenu::Init, "init", "write defaults to disk")
                .item(ConfigMenu::Backend, "backend", backend_hint.as_str())
                .item(ConfigMenu::Route, "route", route_hint.as_str())
                .item(ConfigMenu::Ensemble, "ensemble", ensemble_hint.as_str())
                .item(ConfigMenu::Back, "back", "return to main menu")
                .interact(),
        )?;

        let menu_item = match nav {
            // Esc inside config = back to main menu
            Nav::Back => break,
            Nav::Value(m) => m,
        };

        if menu_item == ConfigMenu::Back {
            break;
        }

        if let Err(e) = dispatch_config(menu_item).await {
            let _ = cliclack::note("error", format!("{e:#}"));
        }
    }

    // Return to caller (run_main loop continues)
    Ok(())
}

async fn dispatch_config(item: ConfigMenu) -> anyhow::Result<()> {
    match item {
        ConfigMenu::Show => super::config::run(ConfigAction::Show).await,
        ConfigMenu::Init => {
            let nav = prompt(
                cliclack::confirm("Overwrite if config already exists?")
                    .initial_value(false)
                    .interact(),
            )?;
            match nav {
                Nav::Back => Ok(()),
                Nav::Value(force) => super::config::run(ConfigAction::Init { force }).await,
            }
        }
        ConfigMenu::Backend => match pick_backend("Backend to configure")? {
            None => Ok(()),
            Some(id) => super::config::run(ConfigAction::Backend { id }).await,
        },
        ConfigMenu::Route => match pick_route_key("Tool to set")? {
            None => Ok(()),
            Some(tool) => {
                super::config::run(ConfigAction::Route {
                    tool,
                    backend: None,
                })
                .await
            }
        },
        ConfigMenu::Ensemble => match pick_ensemble_target("Target")? {
            None => Ok(()),
            Some(target) => super::config::run(ConfigAction::Ensemble { target }).await,
        },
        ConfigMenu::Back => Ok(()),
    }
}

// ─── Flows ───────────────────────────────────────────────────────────────────

async fn rescue_flow() -> anyhow::Result<()> {
    let nav = prompt(
        cliclack::input("Task to rescue")
            .placeholder("e.g. \"list files in src/ and explain main.rs\"")
            .interact(),
    )?;
    match nav {
        Nav::Back => Ok(()),
        Nav::Value(task) => super::rescue::run(task, None, None, true).await,
    }
}

async fn review_flow() -> anyhow::Result<()> {
    let nav = prompt(
        cliclack::input("Target to review")
            .placeholder("file path / diff / 'last commit'")
            .interact(),
    )?;
    let target: String = match nav {
        Nav::Back => return Ok(()),
        Nav::Value(v) => v,
    };

    let nav = prompt(
        cliclack::input("Optional reviewer focus (leave blank for default)")
            .default_input("")
            .required(false)
            .interact(),
    )?;
    let focus = match nav {
        Nav::Back => return Ok(()),
        Nav::Value(raw) => {
            let s: String = raw;
            if s.trim().is_empty() { None } else { Some(s) }
        }
    };

    super::review::run(target, focus, None, None, None).await
}

async fn security_review_flow() -> anyhow::Result<()> {
    let nav = prompt(
        cliclack::input("Target to security-review")
            .placeholder("file path / diff / 'last commit'")
            .interact(),
    )?;
    let target: String = match nav {
        Nav::Back => return Ok(()),
        Nav::Value(v) => v,
    };

    let nav = prompt(
        cliclack::input("Optional focus area (leave blank for full checklist)")
            .default_input("")
            .required(false)
            .interact(),
    )?;
    let focus = match nav {
        Nav::Back => return Ok(()),
        Nav::Value(raw) => {
            let s: String = raw;
            if s.trim().is_empty() { None } else { Some(s) }
        }
    };

    super::security_review::run(target, focus, None, None, None).await
}

async fn adversarial_review_flow() -> anyhow::Result<()> {
    let nav = prompt(
        cliclack::input("Target to adversarial-review")
            .placeholder("file path / diff / 'last commit'")
            .interact(),
    )?;
    let target: String = match nav {
        Nav::Back => return Ok(()),
        Nav::Value(v) => v,
    };

    let nav = prompt(
        cliclack::input("Optional attack focus (leave blank for full checklist)")
            .default_input("")
            .required(false)
            .interact(),
    )?;
    let focus = match nav {
        Nav::Back => return Ok(()),
        Nav::Value(raw) => {
            let s: String = raw;
            if s.trim().is_empty() { None } else { Some(s) }
        }
    };

    super::adversarial_review::run(target, focus, None, None, None).await
}

async fn explain_flow() -> anyhow::Result<()> {
    let nav = prompt(
        cliclack::input("Target to explain")
            .placeholder("file path / symbol")
            .interact(),
    )?;
    let target: String = match nav {
        Nav::Back => return Ok(()),
        Nav::Value(v) => v,
    };

    let nav = prompt(
        cliclack::confirm("Deep explanation?")
            .initial_value(false)
            .interact(),
    )?;
    let deep = match nav {
        Nav::Back => return Ok(()),
        Nav::Value(v) => v,
    };

    super::explain::run(target, deep, None, None).await
}

async fn refactor_flow() -> anyhow::Result<()> {
    let nav = prompt(cliclack::input("File or path to refactor").interact())?;
    let path: String = match nav {
        Nav::Back => return Ok(()),
        Nav::Value(v) => v,
    };

    let nav = prompt(
        cliclack::input("Refactor goal")
            .placeholder("e.g. \"extract X into its own module\"")
            .interact(),
    )?;
    let goal: String = match nav {
        Nav::Back => return Ok(()),
        Nav::Value(v) => v,
    };

    super::refactor::run(path, goal, None, None).await
}

async fn ensemble_flow() -> anyhow::Result<()> {
    let nav = prompt(cliclack::input("Prompt to fan out").interact())?;
    match nav {
        Nav::Back => Ok(()),
        Nav::Value(prompt_text) => super::ensemble::run(prompt_text, None, None, None, None).await,
    }
}

async fn login_flow() -> anyhow::Result<()> {
    let backend = match pick_backend("Backend to authenticate")? {
        None => return Ok(()),
        Some(b) => b,
    };

    let nav = prompt(
        cliclack::confirm("Check only (do not launch login)?")
            .initial_value(false)
            .interact(),
    )?;
    let check_only = match nav {
        Nav::Back => return Ok(()),
        Nav::Value(v) => v,
    };

    super::login::run(backend, check_only).await
}

async fn update_flow() -> anyhow::Result<()> {
    let nav = prompt(
        cliclack::confirm("Check only (do not download)?")
            .initial_value(true)
            .interact(),
    )?;
    match nav {
        Nav::Back => Ok(()),
        Nav::Value(check_only) => super::update::run(check_only, false).await,
    }
}

// ─── Pickers (return None = back/cancel) ─────────────────────────────────────

fn pick_backend(label: &str) -> anyhow::Result<Option<BackendId>> {
    let nav = prompt(
        cliclack::select(label)
            .item(
                BackendId::Antigravity,
                "antigravity",
                "agy — Gemini CLI successor",
            )
            .item(BackendId::Claude, "claude", "")
            .item(BackendId::Codex, "codex", "")
            .item(BackendId::Opencode, "opencode", "")
            .item(BackendId::Goose, "goose", "")
            .item(BackendId::Copilot, "copilot", "GitHub Copilot")
            .interact(),
    )?;
    Ok(match nav {
        Nav::Back => None,
        Nav::Value(v) => Some(v),
    })
}

fn pick_route_key(label: &str) -> anyhow::Result<Option<RouteKey>> {
    let nav = prompt(
        cliclack::select(label)
            .item(RouteKey::Rescue, "rescue", "")
            .item(RouteKey::Review, "review", "")
            .item(RouteKey::Explain, "explain", "")
            .item(RouteKey::Refactor, "refactor", "")
            .interact(),
    )?;
    Ok(match nav {
        Nav::Back => None,
        Nav::Value(v) => Some(v),
    })
}

fn pick_ensemble_target(label: &str) -> anyhow::Result<Option<EnsembleTarget>> {
    let nav = prompt(
        cliclack::select(label)
            .item(
                EnsembleTarget::Default,
                "default",
                "agentpit ensemble subcommand",
            )
            .item(EnsembleTarget::Review, "review", "agentpit review")
            .item(
                EnsembleTarget::SecurityReview,
                "security-review",
                "agentpit security-review",
            )
            .item(EnsembleTarget::Rescue, "rescue", "agentpit rescue")
            .item(EnsembleTarget::Refactor, "refactor", "agentpit refactor")
            .interact(),
    )?;
    Ok(match nav {
        Nav::Back => None,
        Nav::Value(v) => Some(v),
    })
}
