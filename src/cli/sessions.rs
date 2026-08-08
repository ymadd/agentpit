//! `agentpit sessions` — list, inspect, and export saved REPL sessions (non-interactive
//! surface; the interactive roster is the TUI's Agents View, design §11.5/B1).

use anyhow::Result;
use clap::Subcommand;
use console::style;

use crate::session::{self, resolve};
use agentpit_events::session::SessionLog;

#[derive(Debug, Subcommand)]
pub enum Action {
    /// List saved sessions, newest first (the default action).
    List {
        /// Emit machine-readable JSON (one object per session).
        #[arg(long)]
        json: bool,
    },
    /// Print a session's conversation (the current branch, summary applied).
    Show {
        /// Session id — unique prefix/suffix accepted.
        id: String,
    },
    /// Print a session's raw JSONL to stdout (full history, all branches).
    Export {
        /// Session id — unique prefix/suffix accepted.
        id: String,
    },
}

pub async fn run(action: Option<Action>) -> Result<()> {
    match action.unwrap_or(Action::List { json: false }) {
        Action::List { json } => list(json),
        Action::Show { id } => show(&id),
        Action::Export { id } => export(&id),
    }
}

fn list(json: bool) -> Result<()> {
    let sessions = session::list_all();
    if json {
        let rows: Vec<serde_json::Value> = sessions
            .iter()
            .map(|m| {
                serde_json::json!({
                    "session_id": m.session_id,
                    "path": m.path.display().to_string(),
                    "title": m.title,
                    "cwd": m.cwd,
                    "updated_at_ms": m
                        .updated_at
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0),
                    "size_bytes": m.size_bytes,
                })
            })
            .collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    if sessions.is_empty() {
        println!(
            "no saved sessions yet. Start one with `agentpit repl` — turns are recorded automatically."
        );
        return Ok(());
    }
    for m in &sessions {
        let tail = &m.session_id[m.session_id.len().saturating_sub(12)..];
        let age = m
            .updated_at
            .elapsed()
            .map(format_age)
            .unwrap_or_else(|_| "?".into());
        println!(
            "{}  {}  {}  {}",
            style(tail).cyan(),
            style(format!("{age:>8}")).dim(),
            m.title.as_deref().unwrap_or("-"),
            style(&m.cwd).dim()
        );
    }
    println!(
        "\n{}",
        style("resume with: agentpit repl --resume <id>").dim()
    );
    Ok(())
}

fn show(id: &str) -> Result<()> {
    let meta = resolve(id)?;
    let log = SessionLog::open(&meta.path)?;
    for w in log.warnings() {
        eprintln!("{} {w}", style("warning:").yellow());
    }
    println!(
        "{} {} ({})\n",
        style("session").bold(),
        log.session_id(),
        meta.cwd
    );
    for item in log.context() {
        match item {
            agentpit_events::session::ContextItem::Summary(t) => {
                println!("{}\n{t}\n", style("── summary ──").magenta());
            }
            agentpit_events::session::ContextItem::User(t) => {
                println!("{} {t}\n", style("you:").green().bold());
            }
            agentpit_events::session::ContextItem::Answer { backend, text } => {
                println!("{} {text}\n", style(format!("{backend}:")).cyan().bold());
            }
        }
    }
    Ok(())
}

fn export(id: &str) -> Result<()> {
    let meta = resolve(id)?;
    print!("{}", std::fs::read_to_string(&meta.path)?);
    Ok(())
}

fn format_age(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86_400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86_400)
    }
}
