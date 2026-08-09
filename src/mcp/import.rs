//! `agentpit mcp import` — copy Claude Code's server list into agentpit's config, once.
//!
//! `~/.claude.json` is another tool's file. agentpit does not read it on any other path:
//! inheriting someone else's spawn list silently is not a thing a CLI should do, and the
//! file also holds that tool's own state. This module is the one, explicit, user-invoked
//! door.
//!
//! Two rules follow from "importing is a config mutation":
//!
//! * **The diff comes first.** [`plan`] is pure — it compares the source against the config
//!   and returns what *would* change. Rendering it is all a bare `agentpit mcp import` does.
//! * **Writing is opt-in.** `--apply` is what turns the plan into a written config. There is
//!   no interactive confirmation, deliberately: this command is reachable from the TUI's
//!   suspended screen, where a prompt on stdin is a hang waiting to happen.
//!
//! An existing entry is never overwritten. Re-importing after editing a command is a no-op
//! on that server, which is the safe direction: agentpit's own config wins over a copy of
//! another tool's.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use console::style;

use crate::config::{HubConfig, McpServerConfig};

use super::servers::{Origin, Rejected, Servers};

/// Claude Code's own config file, in the user's home directory.
pub fn claude_json_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".claude.json")
}

/// `~/.claude.json` is a large file (it carries that tool's history too). Cap the read.
const MAX_SOURCE_BYTES: u64 = 32 * 1024 * 1024;

/// One line of the plan.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// Not in agentpit's config: `--apply` adds it.
    Add(String, McpServerConfig),
    /// Already configured under this name and left alone.
    Keep { name: String, identical: bool },
}

/// What an import would do.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Plan {
    pub changes: Vec<Change>,
    /// Entries in the source that cannot be run as stdio servers.
    pub rejected: Vec<Rejected>,
}

impl Plan {
    pub fn additions(&self) -> impl Iterator<Item = (&String, &McpServerConfig)> {
        self.changes.iter().filter_map(|c| match c {
            Change::Add(name, cfg) => Some((name, cfg)),
            Change::Keep { .. } => None,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.additions().next().is_none()
    }
}

/// Read the server blocks `~/.claude.json` holds for this machine.
///
/// Two places carry them, and both are read: the top-level `mcpServers` (that tool's global
/// scope) and `projects.<cwd>.mcpServers` (the scope for the checkout agentpit is running
/// in). A project entry for a *different* directory is not this project's business and is
/// left alone.
pub fn read_source(path: &Path, cwd: &Path) -> Servers {
    let mut servers = Servers::default();
    match std::fs::metadata(path) {
        Ok(meta) if meta.len() > MAX_SOURCE_BYTES => return servers,
        Ok(_) => {}
        Err(_) => return servers,
    }
    let Ok(raw) = std::fs::read_to_string(path) else {
        return servers;
    };
    servers.extend_from_json(&raw, path, Origin::Config);

    // The project block, if this directory has one. Parsed separately (and leniently: an
    // unknown shape here is simply "no project servers", not a failed import).
    let Ok(value) = serde_json::from_str::<serde_json::Value>(&raw) else {
        return servers;
    };
    let key = cwd.display().to_string();
    if let Some(block) = value.get("projects").and_then(|p| p.get(&key))
        && let Ok(raw_block) = serde_json::to_string(block)
    {
        let mut project = Servers::default();
        project.extend_from_json(&raw_block, path, Origin::Config);
        for def in project.defs {
            if !servers.defs.iter().any(|d| d.name == def.name) {
                servers.defs.push(def);
            }
        }
        servers.rejected.extend(project.rejected);
    }
    servers
}

/// Compare the source against `config` — pure, so the diff shown and the write applied
/// cannot disagree.
pub fn plan(source: Servers, config: &HubConfig) -> Plan {
    let mut plan = Plan {
        rejected: source.rejected,
        ..Default::default()
    };
    for def in source.defs {
        let candidate = McpServerConfig {
            command: def.command,
            args: def.args,
            env: def.env,
            cwd: def.cwd,
            enabled: true,
        };
        match config.mcp.servers.get(&def.name) {
            Some(existing) => plan.changes.push(Change::Keep {
                name: def.name,
                identical: *existing == candidate,
            }),
            None => plan.changes.push(Change::Add(def.name, candidate)),
        }
    }
    plan
}

/// Merge a plan's additions into `config`. Only additions — see the module docs.
pub fn apply(plan: &Plan, config: &mut HubConfig) -> usize {
    let mut added = 0;
    for (name, cfg) in plan.additions() {
        config.mcp.servers.insert(name.clone(), cfg.clone());
        added += 1;
    }
    added
}

/// The report a bare `mcp import` prints, and that `--apply` prints before writing.
pub fn render(plan: &Plan, source: &Path) -> String {
    let mut out = format!("Source: {}\n", source.display());
    if plan.changes.is_empty() && plan.rejected.is_empty() {
        out.push_str("  (no MCP servers found)\n");
        return out;
    }
    for change in &plan.changes {
        match change {
            Change::Add(name, cfg) => {
                let args = if cfg.args.is_empty() {
                    String::new()
                } else {
                    format!(" {}", cfg.args.join(" "))
                };
                let env = if cfg.env.is_empty() {
                    String::new()
                } else {
                    let keys: Vec<&str> = cfg.env.keys().map(String::as_str).collect();
                    format!("   env: {}", keys.join(", "))
                };
                out.push_str(&format!(
                    "  {} {name}  ({}{args}){env}\n",
                    style("+").green(),
                    cfg.command
                ));
            }
            Change::Keep { name, identical } => {
                let why = if *identical {
                    "already configured, identical"
                } else {
                    "already configured with a different command — kept"
                };
                out.push_str(&format!("  {} {name}  ({why})\n", style("=").dim()));
            }
        }
    }
    for r in &plan.rejected {
        out.push_str(&format!(
            "  {} {}  (skipped: {})\n",
            style("-").yellow(),
            r.name,
            r.reason.as_str()
        ));
    }
    out
}

/// `agentpit mcp import [--apply]`.
pub fn run(apply_it: bool, cwd: &Path) -> Result<()> {
    let source = claude_json_path();
    let loaded = crate::config::load_config(None)?;
    let mut config = loaded.config;
    let found = read_source(&source, cwd);
    let plan = plan(found, &config);

    print!("{}", render(&plan, &source));

    if plan.is_empty() {
        println!("\nNothing to import.");
        return Ok(());
    }
    if !apply_it {
        println!(
            "\n{} nothing was written. Re-run with {} to add the {} server(s) above to {}.",
            style("Dry run:").yellow(),
            style("--apply").bold(),
            plan.additions().count(),
            crate::config::default_config_path().display()
        );
        return Ok(());
    }

    let added = apply(&plan, &mut config);
    let path = crate::config::save_config(&config).context("failed to write the config")?;
    println!(
        "\n{} added {added} server(s) to {}. Run `agentpit mcp refresh` to list their prompts.",
        style("Imported:").green(),
        path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    const CLAUDE_JSON: &str = r#"{
      "numStartups": 12,
      "mcpServers": {
        "context7": {"command": "npx", "args": ["-y", "@upstash/context7-mcp"]},
        "remote":   {"type": "http", "url": "https://example.invalid/mcp"}
      },
      "projects": {
        "PROJECT_DIR": {
          "mcpServers": {
            "local-tools": {"command": "./tools/mcp", "env": {"TOKEN": "abc"}}
          }
        },
        "/somewhere/else": {
          "mcpServers": {"not-ours": {"command": "nope"}}
        }
      }
    }"#;

    fn write_source(dir: &Path, cwd: &Path) -> PathBuf {
        let path = dir.join(".claude.json");
        fs::write(
            &path,
            CLAUDE_JSON.replace("PROJECT_DIR", &cwd.display().to_string()),
        )
        .unwrap();
        path
    }

    #[test]
    fn reads_global_and_this_projects_blocks_but_not_another_projects() {
        let dir = tempdir().unwrap();
        let cwd = dir.path().join("checkout");
        let path = write_source(dir.path(), &cwd);
        let found = read_source(&path, &cwd);
        let names: Vec<&str> = found.defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["context7", "local-tools"]);
        assert_eq!(found.rejected.len(), 1, "the http server is not runnable");
        assert_eq!(found.rejected[0].name, "remote");
    }

    #[test]
    fn a_missing_or_junk_source_imports_nothing_and_does_not_panic() {
        let dir = tempdir().unwrap();
        assert_eq!(
            read_source(&dir.path().join("absent.json"), dir.path()).defs,
            Vec::new()
        );
        let junk = dir.path().join("junk.json");
        fs::write(&junk, "not json").unwrap();
        let found = read_source(&junk, dir.path());
        assert!(found.defs.is_empty());
        assert_eq!(found.rejected.len(), 1);
    }

    #[test]
    fn the_plan_adds_new_servers_and_never_overwrites_an_existing_one() {
        let dir = tempdir().unwrap();
        let cwd = dir.path().join("checkout");
        let path = write_source(dir.path(), &cwd);

        let mut config = HubConfig::default();
        config.mcp.servers.insert(
            "context7".to_string(),
            McpServerConfig {
                command: "my-own-context7".into(),
                enabled: true,
                ..Default::default()
            },
        );

        let plan = plan(read_source(&path, &cwd), &config);
        assert_eq!(plan.additions().count(), 1);
        assert!(matches!(
            &plan.changes[0],
            Change::Keep { name, identical: false } if name == "context7"
        ));

        let added = apply(&plan, &mut config);
        assert_eq!(added, 1);
        // The pre-existing definition survived the import untouched.
        assert_eq!(config.mcp.servers["context7"].command, "my-own-context7");
        assert_eq!(config.mcp.servers["local-tools"].command, "./tools/mcp");
        assert_eq!(config.mcp.servers["local-tools"].env["TOKEN"], "abc");

        // Idempotent: a second import of the same source changes nothing.
        let again = plan_for(&path, &cwd, &config);
        assert!(again.is_empty());
        assert_eq!(apply(&again, &mut config), 0);
    }

    fn plan_for(path: &Path, cwd: &Path, config: &HubConfig) -> Plan {
        plan(read_source(path, cwd), config)
    }

    #[test]
    fn the_report_names_every_server_and_why() {
        let dir = tempdir().unwrap();
        let cwd = dir.path().join("checkout");
        let path = write_source(dir.path(), &cwd);
        let rendered = render(&plan_for(&path, &cwd, &HubConfig::default()), &path);
        assert!(rendered.contains("context7"));
        assert!(rendered.contains("local-tools"));
        assert!(
            rendered.contains("remote"),
            "a skipped entry is still named"
        );
        assert!(rendered.contains("not a stdio server"));
        // Env is summarised by key: an imported token must not be echoed to the terminal.
        assert!(rendered.contains("TOKEN"));
        assert!(!rendered.contains("abc"));
    }
}
