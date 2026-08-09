//! `agentpit mcp <action>` — the MCP channel subcommand, in both directions.
//!
//! `serve` runs agentpit AS a server. The other three make agentpit a CLIENT of other
//! servers: `list` reads the cache, `refresh` fills it (the only action that spawns
//! anything), `import` copies Claude Code's server list into agentpit's config once.
//!
//! ## Why the slash surfaces get their own action enum
//!
//! [`Action`] is the CLI's grammar and contains `Serve`. [`SlashAction`] is the interactive
//! surfaces' grammar and does not — a `serve` reachable from `/mcp` would hand the REPL's
//! own stdin/stdout to a JSON-RPC framing mid-session, i.e. break the surface the user typed
//! it on. The absence is structural rather than a check: [`run_words`] parses into a type
//! with no such variant, so there is nothing for a slash line to select. `crate::cli::slash`
//! refuses the word up front too, so the user gets a sentence rather than a clap error.

use anyhow::Result;
use clap::Subcommand;
use console::style;

use crate::mcp::cache::{PromptCache, age_label};
use crate::mcp::servers::{self, ServerDef, Servers};

#[derive(Subcommand, Debug)]
pub enum Action {
    /// Run a stdio MCP server exposing agentpit's dispatch / ensemble tools.
    Serve,
    /// Show the configured servers and their cached prompts. Never spawns a server.
    List,
    /// Connect to each configured server, list its prompts, and update the cache.
    Refresh {
        /// Refresh only this server.
        #[arg(long)]
        server: Option<String>,
    },
    /// Copy Claude Code's MCP servers (~/.claude.json) into agentpit's config.
    Import {
        /// Write the config. Without it the diff is printed and nothing changes.
        #[arg(long)]
        apply: bool,
    },
}

/// The actions an interactive surface may run. Deliberately without `Serve`; see the module
/// docs.
#[derive(Subcommand, Debug)]
pub enum SlashAction {
    /// Show the configured servers and their cached prompts. Never spawns a server.
    List,
    /// Connect to each configured server, list its prompts, and update the cache.
    Refresh {
        /// Refresh only this server.
        #[arg(long)]
        server: Option<String>,
    },
    /// Copy Claude Code's MCP servers (~/.claude.json) into agentpit's config.
    Import {
        /// Write the config. Without it the diff is printed and nothing changes.
        #[arg(long)]
        apply: bool,
    },
}

/// `/mcp …` on an interactive surface, parsed with this subcommand's own clap grammar (see
/// `sessions::run_words` for why the slash surfaces reuse clap instead of re-parsing).
#[derive(clap::Parser, Debug)]
#[command(name = "/mcp", no_binary_name = true)]
struct Words {
    #[command(subcommand)]
    action: Option<SlashAction>,
}

pub async fn run(action: Action) -> Result<()> {
    match action {
        Action::Serve => crate::mcp::serve::run().await,
        Action::List => list(),
        Action::Refresh { server } => refresh(server.as_deref()).await,
        Action::Import { apply } => {
            let cwd = crate::cli::resolve_cwd(None)?;
            crate::mcp::import::run(apply, &cwd)
        }
    }
}

/// Run `/mcp <words>`; no words is a bare `/mcp`, i.e. `list`.
pub async fn run_words(words: Vec<String>) -> Result<()> {
    match <Words as clap::Parser>::try_parse_from(words) {
        Ok(parsed) => match parsed.action {
            None | Some(SlashAction::List) => list(),
            Some(SlashAction::Refresh { server }) => refresh(server.as_deref()).await,
            Some(SlashAction::Import { apply }) => {
                let cwd = crate::cli::resolve_cwd(None)?;
                crate::mcp::import::run(apply, &cwd)
            }
        },
        Err(e) => {
            let _ = e.print();
            Ok(())
        }
    }
}

/// Definitions plus the cache, for `list` and `refresh`.
fn gather() -> Result<(Servers, std::path::PathBuf)> {
    let cwd = crate::cli::resolve_cwd(None)?;
    let loaded = crate::config::load_config(None)?;
    Ok((servers::gather(&loaded.config, &cwd), cwd))
}

/// `agentpit mcp list` — from the cache, with no process started.
fn list() -> Result<()> {
    let (found, cwd) = gather()?;
    let cache = PromptCache::load();

    if found.defs.is_empty() && found.rejected.is_empty() {
        println!(
            "No MCP servers configured. Add a [mcp.servers.<name>] block to {}, drop a {} in \
             {}, or run `agentpit mcp import`.",
            crate::config::default_config_path().display(),
            servers::PROJECT_FILE,
            cwd.display()
        );
        return Ok(());
    }

    for def in &found.defs {
        println!("{}", render_server(def, &cache));
    }
    for r in &found.rejected {
        println!(
            "{} {}  ({} — {})",
            style("-").yellow(),
            r.name,
            r.reason.as_str(),
            r.source.display()
        );
    }
    println!("\nRun `agentpit mcp refresh` to update the cache (this is what starts a server).");
    Ok(())
}

fn render_server(def: &ServerDef, cache: &PromptCache) -> String {
    let head = format!(
        "{} {}  [{}]  {}",
        if def.enabled {
            style("●").green()
        } else {
            style("○").dim()
        },
        style(&def.name).bold(),
        def.origin.label(),
        style(&def.command).dim()
    );
    if !def.enabled {
        return format!("{head}\n    disabled");
    }
    match cache.fresh_for(def) {
        None if cache.servers.contains_key(&def.name) => format!(
            "{head}\n    the definition changed since the last refresh — run `agentpit mcp refresh`"
        ),
        None => format!("{head}\n    never refreshed — run `agentpit mcp refresh`"),
        Some(entry) if entry.prompts.is_empty() => {
            format!(
                "{head}\n    no prompts (refreshed {})",
                age_label(entry.refreshed_at)
            )
        }
        Some(entry) => {
            let mut out = format!(
                "{head}\n    {} prompt(s), refreshed {}",
                entry.prompts.len(),
                age_label(entry.refreshed_at)
            );
            for p in &entry.prompts {
                match crate::mcp::prompts::command_name(&def.name, &p.name) {
                    Some(name) => out.push_str(&format!("\n      /{name}")),
                    // The name a prompt could NOT become a command under is the one most
                    // likely to be hostile — that is why it was refused. It still goes
                    // through the same gate every other outside string reaching a terminal
                    // does, rather than being printed because it is "only" a diagnostic.
                    None => out.push_str(&format!(
                        "\n      ({} — not offerable as a slash command)",
                        crate::cli::skills::render_safe(&p.name, crate::cli::skills::PATH_WIDTH)
                    )),
                }
            }
            out
        }
    }
}

/// `agentpit mcp refresh` — the one action that starts a server process.
async fn refresh(only: Option<&str>) -> Result<()> {
    let (found, _cwd) = gather()?;
    let loaded = crate::config::load_config(None)?;
    let budget = crate::mcp::client::timeout_from(loaded.config.mcp.connect_timeout_secs);

    let targets: Vec<ServerDef> = found
        .defs
        .into_iter()
        .filter(|d| d.enabled && only.is_none_or(|name| d.name == name))
        .collect();

    if targets.is_empty() {
        match only {
            Some(name) => println!("No enabled MCP server named '{name}'."),
            None => println!("No enabled MCP servers to refresh."),
        }
        return Ok(());
    }

    println!(
        "Refreshing {} server(s), up to {}s each…",
        targets.len(),
        budget.as_secs()
    );
    let (outcomes, cache) = crate::mcp::client::refresh_all(&targets, budget).await;
    cache.save()?;

    for outcome in &outcomes {
        match &outcome.result {
            Ok(n) => println!("  {} {}  {n} prompt(s)", style("✓").green(), outcome.name),
            Err(e) => println!("  {} {}  {e:#}", style("✗").red(), outcome.name),
        }
    }
    let ok = outcomes.iter().filter(|o| o.result.is_ok()).count();
    println!(
        "\n{ok}/{} refreshed. New prompts appear as /<server>:<prompt> on the next `agentpit \
         repl` or `agentpit tui`.",
        outcomes.len()
    );
    Ok(())
}

/// Does the argument text of a `/mcp` line name the CLI-only `serve` action?
///
/// Lives here rather than in the slash table so the refusal and the grammar it protects sit
/// in one file.
pub fn is_serve_word(rest: &str) -> bool {
    crate::cli::slash::split_words(rest)
        .first()
        .is_some_and(|w| w.eq_ignore_ascii_case("serve"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The slash grammar has no `serve` to select — proven against clap itself, not against
    /// a hand-written list of allowed words.
    #[test]
    fn the_slash_grammar_cannot_express_serve() {
        assert!(
            <Words as clap::Parser>::try_parse_from(vec!["serve".to_string()]).is_err(),
            "/mcp serve must not parse on an interactive surface"
        );
        for good in [vec!["list"], vec!["refresh"], vec!["import"]] {
            let words: Vec<String> = good.iter().map(|s| s.to_string()).collect();
            assert!(
                <Words as clap::Parser>::try_parse_from(words).is_ok(),
                "{good:?} must parse"
            );
        }
        // No words at all is a bare `/mcp`.
        assert!(<Words as clap::Parser>::try_parse_from(Vec::<String>::new()).is_ok());
        // Flags the actions do take.
        assert!(
            <Words as clap::Parser>::try_parse_from(vec![
                "import".to_string(),
                "--apply".to_string()
            ])
            .is_ok()
        );
        assert!(
            <Words as clap::Parser>::try_parse_from(vec![
                "refresh".to_string(),
                "--server".to_string(),
                "ctx7".to_string()
            ])
            .is_ok()
        );
    }

    #[test]
    fn serve_is_recognised_however_it_is_typed() {
        assert!(is_serve_word("serve"));
        assert!(is_serve_word("  Serve  "));
        assert!(is_serve_word("SERVE --whatever"));
        assert!(!is_serve_word(""));
        assert!(!is_serve_word("list"));
        assert!(!is_serve_word("server"));
    }

    #[test]
    fn cli_grammar_still_has_serve() {
        // The CLI keeps it: `agentpit mcp serve` is how a manager launches agentpit.
        #[derive(clap::Parser, Debug)]
        #[command(name = "mcp", no_binary_name = true)]
        struct CliWords {
            #[command(subcommand)]
            action: Action,
        }
        assert!(<CliWords as clap::Parser>::try_parse_from(vec!["serve".to_string()]).is_ok());
    }
}
