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
        "adversarial-review.md",
        include_str!("../../commands/agentpit/adversarial-review.md"),
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
    ("login.md", include_str!("../../commands/agentpit/login.md")),
    (
        "workflow.md",
        include_str!("../../commands/agentpit/workflow.md"),
    ),
    ("mcp.md", include_str!("../../commands/agentpit/mcp.md")),
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
        "agentpit-adversarial-review.md",
        include_str!("../../skills/agentpit-adversarial-review.md"),
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
    (
        "agentpit-workflow.md",
        include_str!("../../skills/agentpit-workflow.md"),
    ),
    (
        "agentpit-mcp.md",
        include_str!("../../skills/agentpit-mcp.md"),
    ),
];

/// Compile-time assertion: COMMAND_FILES and SKILL_FILES must stay in lockstep.
const _: () = assert!(
    COMMAND_FILES.len() == SKILL_FILES.len(),
    "COMMAND_FILES and SKILL_FILES must have the same number of entries"
);

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
    Ok((base.join("commands").join("agentpit"), base.join("skills")))
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

pub async fn run(scope: Option<Scope>, force: bool, refresh: bool, json: bool) -> Result<()> {
    if refresh {
        return run_refresh(json).await;
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

/// What a refresh did to one `.claude/` install. Serialized as-is by `--json`, which the
/// desktop app reads to report skill/command updates instead of guessing.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct RefreshedScope {
    /// "project" or "user".
    pub scope: String,
    /// The `.claude/` root this scope resolved to.
    pub path: String,
    /// Files rewritten because their content differed (or was missing).
    pub refreshed: usize,
    /// Files the install is expected to carry in total.
    pub total: usize,
}

/// The whole refresh: one entry per detected install. An empty list means no `.claude/`
/// directory exists yet, which is not an error — nothing was installed to refresh.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct RefreshReport {
    pub scopes: Vec<RefreshedScope>,
}

impl RefreshReport {
    pub fn total_refreshed(&self) -> usize {
        self.scopes.iter().map(|s| s.refreshed).sum()
    }
}

/// Rewrite the managed command/skill files in every detected `.claude/` install so they
/// match this binary's embedded copies. Returns what changed; printing is the caller's job.
pub async fn refresh_existing_installs() -> Result<RefreshReport> {
    let mut report = RefreshReport::default();
    for scope in [Scope::Project, Scope::User] {
        let (commands_dir, skills_dir) = resolve_dirs(scope)?;
        let exists =
            fs::metadata(&commands_dir).await.is_ok() || fs::metadata(&skills_dir).await.is_ok();
        if !exists {
            continue;
        }
        let refreshed = refresh_dir(&commands_dir, COMMAND_FILES).await?
            + refresh_dir(&skills_dir, SKILL_FILES).await?;
        report.scopes.push(RefreshedScope {
            scope: scope_label(scope).to_string(),
            path: commands_dir
                .parent()
                .and_then(|p| p.parent())
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            refreshed,
            total: COMMAND_FILES.len() + SKILL_FILES.len(),
        });
    }
    Ok(report)
}

/// `agentpit init --refresh [--json]`: refresh, then report for humans or machines.
pub async fn run_refresh(json: bool) -> Result<()> {
    let report = refresh_existing_installs().await?;
    if json {
        println!("{}", serde_json::to_string(&report)?);
        return Ok(());
    }
    if report.scopes.is_empty() {
        println!("no .claude/ install detected — run `agentpit init` first");
        return Ok(());
    }
    for scope in &report.scopes {
        if scope.refreshed > 0 {
            println!(
                "refreshed {} of {} files in {} scope ({})",
                scope.refreshed, scope.total, scope.scope, scope.path
            );
        } else {
            println!(
                "{} scope already up to date ({} files, {})",
                scope.scope, scope.total, scope.path
            );
        }
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

async fn install_files(dir: &Path, files: &[(&str, &str)], force: bool, label: &str) -> Result<()> {
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
    fn embeds_eleven_commands_and_eleven_skills() {
        assert_eq!(COMMAND_FILES.len(), 11);
        assert_eq!(SKILL_FILES.len(), 11);
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

    /// `refresh_dir` is what makes an install converge on this binary's copies: it rewrites
    /// only what differs, adds files a newer build introduced, and leaves a matching install
    /// untouched (so the desktop can report "already up to date" honestly).
    #[tokio::test]
    async fn refresh_dir_writes_only_what_differs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let files: &[(&str, &str)] = &[("a.md", "alpha"), ("b.md", "beta")];

        // A directory that does not exist yet is not an install — nothing is created.
        let absent = root.join("absent");
        assert_eq!(refresh_dir(&absent, files).await.unwrap(), 0);
        assert!(!absent.exists());

        // Stale content and a missing file both get written.
        let install = root.join("install");
        std::fs::create_dir_all(&install).unwrap();
        std::fs::write(install.join("a.md"), "OLD").unwrap();
        assert_eq!(refresh_dir(&install, files).await.unwrap(), 2);
        assert_eq!(
            std::fs::read_to_string(install.join("a.md")).unwrap(),
            "alpha"
        );
        assert_eq!(
            std::fs::read_to_string(install.join("b.md")).unwrap(),
            "beta"
        );

        // Second pass has nothing to do.
        assert_eq!(refresh_dir(&install, files).await.unwrap(), 0);
    }

    /// The JSON contract the desktop app parses. Field names are load-bearing.
    #[test]
    fn refresh_report_serializes_the_shape_the_desktop_reads() {
        let report = RefreshReport {
            scopes: vec![RefreshedScope {
                scope: "user".into(),
                path: "/home/x/.claude".into(),
                refreshed: 3,
                total: 22,
            }],
        };
        assert_eq!(report.total_refreshed(), 3);
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"scope\":\"user\""), "got: {json}");
        assert!(json.contains("\"refreshed\":3"), "got: {json}");
        assert!(json.contains("\"total\":22"), "got: {json}");
        // Round-trips, so the desktop parsing the same struct cannot drift.
        assert_eq!(
            serde_json::from_str::<RefreshReport>(&json).unwrap(),
            report
        );
    }
}
