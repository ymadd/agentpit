use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use clap::ValueEnum;
use console::style;
use tokio::fs;

pub(crate) const COMMAND_FILES: &[(&str, &str)] = &[
    (
        "rescue.md",
        include_str!("../../commands/agentpit/rescue.md"),
    ),
    (
        "review.md",
        include_str!("../../commands/agentpit/review.md"),
    ),
    (
        "security-review.md",
        include_str!("../../commands/agentpit/security-review.md"),
    ),
    (
        "explain.md",
        include_str!("../../commands/agentpit/explain.md"),
    ),
    (
        "refactor.md",
        include_str!("../../commands/agentpit/refactor.md"),
    ),
    (
        "ensemble.md",
        include_str!("../../commands/agentpit/ensemble.md"),
    ),
    (
        "status.md",
        include_str!("../../commands/agentpit/status.md"),
    ),
    (
        "login.md",
        include_str!("../../commands/agentpit/login.md"),
    ),
];

pub(crate) const SKILL_FILES: &[(&str, &str)] = &[
    (
        "agentpit-rescue.md",
        include_str!("../../skills/agentpit-rescue.md"),
    ),
    (
        "agentpit-review.md",
        include_str!("../../skills/agentpit-review.md"),
    ),
    (
        "agentpit-security-review.md",
        include_str!("../../skills/agentpit-security-review.md"),
    ),
    (
        "agentpit-explain.md",
        include_str!("../../skills/agentpit-explain.md"),
    ),
    (
        "agentpit-refactor.md",
        include_str!("../../skills/agentpit-refactor.md"),
    ),
    (
        "agentpit-ensemble.md",
        include_str!("../../skills/agentpit-ensemble.md"),
    ),
    (
        "agentpit-status.md",
        include_str!("../../skills/agentpit-status.md"),
    ),
    (
        "agentpit-login.md",
        include_str!("../../skills/agentpit-login.md"),
    ),
];

#[derive(Copy, Clone, Debug, ValueEnum, PartialEq, Eq)]
pub enum Scope {
    /// Install into this directory's ./.claude/ (current project only)
    Project,
    /// Install into ~/.claude/ (all projects)
    User,
}

impl Scope {
    fn base(self) -> Result<PathBuf> {
        match self {
            Scope::Project => Ok(std::env::current_dir()
                .context("current directory unavailable")?
                .join(".claude")),
            Scope::User => Ok(dirs::home_dir()
                .ok_or_else(|| anyhow!("no $HOME"))?
                .join(".claude")),
        }
    }
}

pub(crate) fn resolve_dirs(scope: Scope) -> Result<(PathBuf, PathBuf)> {
    let base = scope.base()?;
    Ok((
        base.join("commands").join("agentpit"),
        base.join("skills"),
    ))
}

fn prompt_scope() -> Result<Scope> {
    let project_dir = std::env::current_dir()
        .ok()
        .map(|p| format!("{}/.claude/", p.display()))
        .unwrap_or_else(|| "./.claude/".into());
    let home_dir = dirs::home_dir()
        .map(|h| format!("{}/.claude/", h.display()))
        .unwrap_or_else(|| "~/.claude/".into());

    cliclack::intro(style(" agentpit init ").on_cyan().black())
        .map_err(|e| anyhow!("intro failed: {e}"))?;
    let scope = cliclack::select("Where should agentpit install its commands and skills?")
        .item(Scope::Project, "Project", &project_dir)
        .item(Scope::User, "User", &home_dir)
        .interact()
        .map_err(|e| anyhow!("selection failed: {e}"))?;
    Ok(scope)
}

pub async fn run(scope: Option<Scope>, force: bool, refresh: bool) -> Result<()> {
    if refresh {
        return refresh_existing_installs().await;
    }

    let scope = match scope {
        Some(s) => s,
        None if std::io::stdin().is_terminal() => prompt_scope()?,
        None => anyhow::bail!("--scope <project|user> is required when stdin is not a TTY"),
    };

    let (commands_dir, skills_dir) = resolve_dirs(scope)?;

    install_files(&commands_dir, COMMAND_FILES, force, "command").await?;
    install_files(&skills_dir, SKILL_FILES, force, "skill").await?;

    let summary = format!(
        "{} commands → {}\n{} skills   → {}",
        COMMAND_FILES.len(),
        commands_dir.display(),
        SKILL_FILES.len(),
        skills_dir.display(),
    );

    if std::io::stdin().is_terminal() {
        cliclack::outro_note(
            format!("Installed to {} scope", style(scope_label(scope)).cyan()),
            summary,
        )
        .map_err(|e| anyhow!("outro failed: {e}"))?;
    } else {
        println!();
        println!("{summary}");
    }
    Ok(())
}

pub async fn refresh_existing_installs() -> Result<()> {
    let mut touched = 0usize;
    for scope in [Scope::Project, Scope::User] {
        let (commands_dir, skills_dir) = resolve_dirs(scope)?;
        let exists = fs::metadata(&commands_dir).await.is_ok()
            || fs::metadata(&skills_dir).await.is_ok();
        if !exists {
            continue;
        }
        let updated = refresh_dir(&commands_dir, COMMAND_FILES).await?
            + refresh_dir(&skills_dir, SKILL_FILES).await?;
        if updated > 0 {
            println!(
                "refreshed {} files in {} scope ({})",
                updated,
                scope_label(scope),
                commands_dir.parent().map(|p| p.display().to_string()).unwrap_or_default(),
            );
        }
        touched += 1;
    }
    if touched == 0 {
        println!("no .claude/ install detected — run `agentpit init` first");
    }
    Ok(())
}

async fn refresh_dir(dir: &Path, files: &[(&str, &str)]) -> Result<usize> {
    if fs::metadata(dir).await.is_err() {
        return Ok(0);
    }
    let mut changed = 0;
    for (name, content) in files {
        let dest = dir.join(name);
        let needs_write = match fs::read(&dest).await {
            Ok(existing) => existing != content.as_bytes(),
            Err(_) => true,
        };
        if needs_write {
            fs::write(&dest, content)
                .await
                .with_context(|| format!("failed to refresh {}", dest.display()))?;
            changed += 1;
        }
    }
    Ok(changed)
}

fn scope_label(scope: Scope) -> &'static str {
    match scope {
        Scope::Project => "project",
        Scope::User => "user",
    }
}

async fn install_files(
    dir: &Path,
    files: &[(&str, &str)],
    force: bool,
    label: &str,
) -> Result<()> {
    fs::create_dir_all(dir)
        .await
        .with_context(|| format!("failed to create {}", dir.display()))?;

    for (name, content) in files {
        let dest = dir.join(name);
        if !force && fs::metadata(&dest).await.is_ok() {
            println!("skip (exists): {}", dest.display());
            continue;
        }
        fs::write(&dest, content)
            .await
            .with_context(|| format!("failed to write {label} {}", dest.display()))?;
        println!("wrote: {}", dest.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeds_eight_commands_and_eight_skills() {
        assert_eq!(COMMAND_FILES.len(), 8);
        assert_eq!(SKILL_FILES.len(), 8);
        for (name, content) in COMMAND_FILES.iter().chain(SKILL_FILES.iter()) {
            assert!(name.ends_with(".md"), "{name} must be a .md file");
            assert!(!content.is_empty(), "{name} content must be non-empty");
            assert!(
                content.starts_with("---"),
                "{name} must start with YAML frontmatter"
            );
        }
    }

    #[test]
    fn project_scope_uses_cwd_dot_claude() {
        let (commands, skills) = resolve_dirs(Scope::Project).unwrap();
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(commands, cwd.join(".claude/commands/agentpit"));
        assert_eq!(skills, cwd.join(".claude/skills"));
    }

    #[test]
    fn user_scope_uses_home_dot_claude() {
        let (commands, skills) = resolve_dirs(Scope::User).unwrap();
        let home = dirs::home_dir().unwrap();
        assert_eq!(commands, home.join(".claude/commands/agentpit"));
        assert_eq!(skills, home.join(".claude/skills"));
    }
}
