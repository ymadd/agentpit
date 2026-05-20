use std::path::Path;

use anyhow::{Context, Result, anyhow};
use tokio::fs;

const COMMAND_FILES: &[(&str, &str)] = &[
    (
        "rescue.md",
        include_str!("../../commands/agentpit/rescue.md"),
    ),
    (
        "review.md",
        include_str!("../../commands/agentpit/review.md"),
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

const SKILL_FILES: &[(&str, &str)] = &[
    (
        "agentpit-rescue.md",
        include_str!("../../skills/agentpit-rescue.md"),
    ),
    (
        "agentpit-review.md",
        include_str!("../../skills/agentpit-review.md"),
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

pub async fn run(force: bool) -> Result<()> {
    let home = dirs::home_dir().ok_or_else(|| anyhow!("no $HOME"))?;
    let commands_dir = home.join(".claude").join("commands").join("agentpit");
    let skills_dir = home.join(".claude").join("skills");

    install_files(&commands_dir, COMMAND_FILES, force, "command").await?;
    install_files(&skills_dir, SKILL_FILES, force, "skill").await?;

    println!();
    println!("Installed {} commands to {}", COMMAND_FILES.len(), commands_dir.display());
    println!("Installed {} skills to {}", SKILL_FILES.len(), skills_dir.display());
    println!();
    println!("Tip: run `agentpit status` to verify backend availability.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embeds_seven_commands_and_seven_skills() {
        assert_eq!(COMMAND_FILES.len(), 7);
        assert_eq!(SKILL_FILES.len(), 7);
        for (name, content) in COMMAND_FILES.iter().chain(SKILL_FILES.iter()) {
            assert!(name.ends_with(".md"), "{name} must be a .md file");
            assert!(!content.is_empty(), "{name} content must be non-empty");
            assert!(
                content.starts_with("---"),
                "{name} must start with YAML frontmatter"
            );
        }
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
