//! Installed agent CLI discovery and self-update commands for the desktop app.
//!
//! The app never accepts a command or arguments from the frontend. It maps a fixed CLI id to a
//! fixed binary and that CLI's own updater, avoiding shell parsing and command injection. Version
//! checks use the resolved executable path so the version shown is the one that will be updated.

use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Serialize;

const MAX_DISPLAY_OUTPUT: usize = 4 * 1024;

#[derive(Clone, Copy)]
struct CliDefinition {
    id: &'static str,
    label: &'static str,
    command: &'static str,
    version_args: &'static [&'static str],
    update_args: Option<&'static [&'static str]>,
    /// Gemini only gained its self-update subcommand in newer releases. Checking help prevents an
    /// older build from interpreting `gemini update` as an interactive prompt.
    update_help_marker: Option<&'static str>,
}

const CLIS: &[CliDefinition] = &[
    CliDefinition {
        id: "claude",
        label: "Claude Code",
        command: "claude",
        version_args: &["--version"],
        update_args: Some(&["update"]),
        update_help_marker: None,
    },
    CliDefinition {
        id: "codex",
        label: "Codex",
        command: "codex",
        version_args: &["--version"],
        update_args: Some(&["update"]),
        update_help_marker: None,
    },
    CliDefinition {
        id: "gemini",
        label: "Gemini CLI",
        command: "gemini",
        version_args: &["--version"],
        update_args: Some(&["update"]),
        update_help_marker: Some("gemini update"),
    },
    CliDefinition {
        id: "antigravity",
        label: "Antigravity",
        command: "agy",
        version_args: &["--version"],
        update_args: Some(&["update"]),
        update_help_marker: None,
    },
    CliDefinition {
        id: "opencode",
        label: "OpenCode",
        command: "opencode",
        version_args: &["--version"],
        update_args: Some(&["upgrade"]),
        update_help_marker: None,
    },
    CliDefinition {
        id: "prime-agent",
        label: "Prime Agent",
        command: "prime-agent",
        version_args: &["--version"],
        update_args: Some(&["update"]),
        update_help_marker: None,
    },
];

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCliInfo {
    id: String,
    label: String,
    command: String,
    installed: bool,
    version: Option<String>,
    path: Option<String>,
    can_update: bool,
    update_command: Option<String>,
    note: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCliUpdate {
    cli: AgentCliInfo,
    output: String,
}

pub fn list() -> Vec<AgentCliInfo> {
    CLIS.iter().map(inspect).collect()
}

pub fn update(id: &str) -> Result<AgentCliUpdate, String> {
    let definition = CLIS
        .iter()
        .find(|definition| definition.id == id)
        .ok_or_else(|| format!("unknown agent CLI: {id}"))?;
    let before = inspect(definition);
    let path = before
        .path
        .as_deref()
        .map(PathBuf::from)
        .ok_or_else(|| format!("{} is not installed", definition.label))?;
    let args = definition
        .update_args
        .ok_or_else(|| format!("{} does not provide a supported updater", definition.label))?;
    if !before.can_update {
        return Err(before.note.unwrap_or_else(|| {
            format!("{} does not provide a supported updater", definition.label)
        }));
    }

    let output = Command::new(&path)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|error| {
            format!(
                "failed to run {}: {error}",
                display_command(definition, args)
            )
        })?;
    let combined = combined_output(&output.stdout, &output.stderr);
    if !output.status.success() {
        let detail = if combined.is_empty() {
            "no output".to_string()
        } else {
            combined
        };
        return Err(format!(
            "{} failed with {}: {}",
            display_command(definition, args),
            output
                .status
                .code()
                .map(|code| code.to_string())
                .unwrap_or_else(|| "signal".into()),
            detail
        ));
    }

    Ok(AgentCliUpdate {
        cli: inspect(definition),
        output: if combined.is_empty() {
            format!(
                "{} finished successfully",
                display_command(definition, args)
            )
        } else {
            combined
        },
    })
}

fn inspect(definition: &CliDefinition) -> AgentCliInfo {
    let Some(path) = resolve_command(definition.command) else {
        return AgentCliInfo {
            id: definition.id.into(),
            label: definition.label.into(),
            command: definition.command.into(),
            installed: false,
            version: None,
            path: None,
            can_update: false,
            update_command: definition
                .update_args
                .map(|args| display_command(definition, args)),
            note: Some(format!("{} が PATH に見つかりません", definition.command)),
        };
    };

    let version = command_text(&path, definition.version_args).filter(|text| !text.is_empty());
    let supports_update = version.is_some()
        && definition.update_args.is_some()
        && definition.update_help_marker.is_none_or(|marker| {
            command_text(&path, &["--help"]).is_some_and(|help| help.contains(marker))
        });
    let note = if version.is_none() {
        Some("バージョンを取得できませんでした".into())
    } else if !supports_update && definition.update_args.is_some() {
        Some("このバージョンはアプリ内更新に対応していません".into())
    } else {
        None
    };

    AgentCliInfo {
        id: definition.id.into(),
        label: definition.label.into(),
        command: definition.command.into(),
        installed: true,
        version,
        path: Some(path.to_string_lossy().into_owned()),
        can_update: supports_update,
        update_command: definition
            .update_args
            .map(|args| display_command(definition, args)),
        note,
    }
}

fn display_command(definition: &CliDefinition, args: &[&str]) -> String {
    std::iter::once(definition.command)
        .chain(args.iter().copied())
        .collect::<Vec<_>>()
        .join(" ")
}

fn command_text(path: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new(path)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = combined_output(&output.stdout, &output.stderr);
    (!text.is_empty()).then_some(text)
}

fn combined_output(stdout: &[u8], stderr: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(stderr);
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(stderr);
    }
    truncate_output(&text)
}

fn truncate_output(text: &str) -> String {
    if text.len() <= MAX_DISPLAY_OUTPUT {
        return text.to_string();
    }
    let mut end = MAX_DISPLAY_OUTPUT;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n… output truncated", &text[..end])
}

pub(crate) fn resolve_command(command: &str) -> Option<PathBuf> {
    let home = dirs_home();
    resolve_command_with(command, env::var_os("PATH").as_deref(), home.as_deref())
}

fn dirs_home() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from)
}

fn resolve_command_with(
    command: &str,
    path_env: Option<&std::ffi::OsStr>,
    home: Option<&Path>,
) -> Option<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(path_env) = path_env {
        dirs.extend(env::split_paths(path_env));
    }
    if let Some(home) = home {
        // GUI apps launched from Finder often receive a minimal PATH. These are the locations used
        // by the supported installers and version managers.
        dirs.extend([
            home.join(".local/bin"),
            home.join(".opencode/bin"),
            home.join(".cargo/bin"),
            home.join(".local/share/mise/shims"),
        ]);
    }
    dirs.extend([
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ]);

    dirs.into_iter()
        .map(|dir| dir.join(command))
        .find(|candidate| candidate.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::ffi::OsString;

    #[test]
    fn definitions_have_unique_ids_and_commands() {
        let ids: HashSet<_> = CLIS.iter().map(|definition| definition.id).collect();
        assert_eq!(ids.len(), CLIS.len());
        assert!(CLIS
            .iter()
            .all(|definition| definition.update_args.is_some()));
    }

    #[test]
    fn inventory_preserves_the_supported_cli_order() {
        let inventory = list();
        let ids: Vec<_> = inventory.iter().map(|cli| cli.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "claude",
                "codex",
                "gemini",
                "antigravity",
                "opencode",
                "prime-agent"
            ]
        );
    }

    #[test]
    fn resolver_uses_path_before_gui_fallbacks() {
        let temp = tempfile::tempdir().unwrap();
        let binary = temp.path().join("agent-test");
        std::fs::write(&binary, b"test").unwrap();
        let path_env = OsString::from(temp.path());
        assert_eq!(
            resolve_command_with("agent-test", Some(&path_env), None),
            Some(binary)
        );
    }

    #[test]
    fn output_truncation_preserves_utf8_boundary() {
        let text = "あ".repeat(MAX_DISPLAY_OUTPUT);
        let truncated = truncate_output(&text);
        assert!(truncated.ends_with("… output truncated"));
        assert!(std::str::from_utf8(truncated.as_bytes()).is_ok());
    }

    #[test]
    fn unknown_cli_cannot_be_executed() {
        let error = update("made-up").unwrap_err();
        assert_eq!(error, "unknown agent CLI: made-up");
    }
}
