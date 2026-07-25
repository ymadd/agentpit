//! Exec adapter for the workflow MANAGER leg.
//!
//! The manager is a regular backend CLI (claude or codex) launched with maximum autonomy so it
//! can drive the workflow by shelling out to `agentpit` from its own Bash tool. It carries the
//! guard's [`child_env`](crate::workflow::guard::child_env) so every nested `agentpit` it spawns
//! inherits the incremented recursion depth — the Rust-enforced ceiling against runaway fan-out.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{AutonomyLevel, ExecAdapter, ExecSpec, StreamFormat};
use crate::types::BackendId;

/// Manager exec adapter. Holds the resolved manager backend and the guard env to inject.
pub struct WorkflowManagerExec {
    pub backend: BackendId,
    pub child_env: Vec<(String, String)>,
    /// When set (claude manager + MCP mode), the manager is launched with `--mcp-config <path>`
    /// and scoped to `mcp__agentpit__*` instead of the Bash allowlist. `None` keeps the Phase-1
    /// shell-out mode exactly as before.
    pub mcp_config_path: Option<PathBuf>,
}

/// RAII guard around the temp MCP config JSON handed to claude via `--mcp-config`.
///
/// The file points claude at `agentpit mcp serve` and is removed on drop, so a workflow run
/// never leaks config files into the temp dir. The owner (the workflow `run`) holds the guard
/// for the whole manager dispatch so the file stays present while claude reads it.
pub struct McpConfigGuard {
    path: PathBuf,
}

impl McpConfigGuard {
    /// Write a temp MCP config pointing claude at `<self_path> mcp serve`. `tag` (e.g. the run
    /// id) keeps the filename unique across concurrent workflows.
    pub fn write(self_path: &str, tag: &str) -> Result<Self> {
        let path = std::env::temp_dir().join(format!("agentpit-mcp-{tag}.json"));
        let config = serde_json::json!({
            "mcpServers": {
                "agentpit": {
                    "command": self_path,
                    "args": ["mcp", "serve"],
                }
            }
        });
        let body = serde_json::to_string_pretty(&config)
            .context("failed to serialize MCP config for the workflow manager")?;
        write_private(&path, &body)
            .with_context(|| format!("failed to write MCP config {}", path.display()))?;
        Ok(Self { path })
    }

    /// Path to the on-disk config, to pass as `--mcp-config <path>`.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for McpConfigGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Write `body` to `path` with owner-only permissions, failing if the path already exists.
///
/// The temp MCP config carries the agentpit binary path and the server command line. Plain
/// `std::fs::write` inherits the process umask (typically world-readable `0644`), exposing those
/// to any local user for the whole manager dispatch. On Unix we create the file `0o600` and use
/// `create_new` so a pre-existing path (a stale crash artifact or a planted file) is never
/// truncated or followed — closing both the disclosure and a TOCTOU race. On non-Unix targets
/// `create_new` still removes the truncate-an-attacker's-file race; permissions follow the
/// platform default.
fn write_private(path: &Path, body: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(body.as_bytes())
}

impl ExecAdapter for WorkflowManagerExec {
    fn id(&self) -> BackendId {
        self.backend
    }

    fn build_spec(&self, task: &str, model: Option<&str>) -> ExecSpec {
        match self.backend {
            // `bypassPermissions` is required so the manager can run `agentpit` via its Bash tool
            // non-interactively — `acceptEdits` only auto-accepts file edits, not Bash commands.
            // The `--allowedTools` list scopes the manager to Bash/Read/Glob/Grep; Bash is the
            // load-bearing entry (it is how the manager invokes `agentpit`) and intentionally
            // grants full shell access, so this is a tool-surface restriction, NOT a sandbox.
            //
            // The task is passed on STDIN, not as a trailing positional: Claude's `--allowedTools`
            // is variadic and greedily consumes following tokens, so a trailing positional prompt
            // gets swallowed into the tool list ("Input must be provided ... when using --print").
            // `--print` reads the prompt from stdin instead.
            BackendId::Claude => {
                let mut args = vec![
                    "--print".into(),
                    "--output-format".into(),
                    "stream-json".into(),
                    "--include-partial-messages".into(),
                    "--verbose".into(),
                    "--permission-mode".into(),
                    "bypassPermissions".into(),
                ];
                // `--model` must precede the variadic `--allowedTools` (which greedily consumes
                // following tokens), so inject it here before the mode-specific tool flags.
                if let Some(m) = model {
                    args.push("--model".into());
                    args.push(m.to_string());
                }
                match &self.mcp_config_path {
                    // MCP mode: the manager orchestrates via the agentpit MCP server's tools
                    // instead of shelling out. Point claude at the temp server config and scope
                    // it to exactly those tools (no Bash). `bypassPermissions` still auto-approves
                    // the MCP tool calls non-interactively.
                    Some(path) => {
                        args.push("--mcp-config".into());
                        args.push(path.display().to_string());
                        args.push("--allowedTools".into());
                        args.push("mcp__agentpit__*".into());
                    }
                    // CLI (shell-out) mode — exactly as Phase 1: Bash is the load-bearing entry
                    // (it is how the manager invokes `agentpit`).
                    None => {
                        args.push("--allowedTools".into());
                        args.push("Bash".into());
                        args.push("--allowedTools".into());
                        args.push("Read".into());
                        args.push("--allowedTools".into());
                        args.push("Glob".into());
                        args.push("--allowedTools".into());
                        args.push("Grep".into());
                    }
                }
                ExecSpec {
                    command: "claude".into(),
                    args,
                    env: self.child_env.clone(),
                    stdin_input: Some(task.to_string()),
                }
            }
            // `--sandbox danger-full-access` lets codex run the agentpit sub-commands.
            BackendId::Codex => {
                let mut args = vec![
                    "exec".into(),
                    "--skip-git-repo-check".into(),
                    "--json".into(),
                    "--sandbox".into(),
                    "danger-full-access".into(),
                ];
                if let Some(m) = model {
                    args.push("--model".into());
                    args.push(m.to_string());
                }
                args.push("-".into());
                ExecSpec {
                    command: "codex".into(),
                    args,
                    env: self.child_env.clone(),
                    stdin_input: Some(task.to_string()),
                }
            }
            // The dangerous full-autonomy posture is isolated to exactly the two supported
            // managers above. Every other backend must be rejected by `is_supported_manager`
            // before this adapter is ever constructed; reaching here means that gate was
            // bypassed, so fail loudly rather than silently mis-classifying as codex.
            other => unreachable!(
                "WorkflowManagerExec built with unsupported manager backend {other:?}; \
                 call is_supported_manager() before constructing this adapter"
            ),
        }
    }

    fn stream_format(&self) -> StreamFormat {
        match self.backend {
            BackendId::Claude => StreamFormat::ClaudeJsonl,
            BackendId::Codex => StreamFormat::CodexJsonl,
            other => unreachable!(
                "WorkflowManagerExec built with unsupported manager backend {other:?}; \
                 call is_supported_manager() before constructing this adapter"
            ),
        }
    }

    fn autonomy(&self) -> AutonomyLevel {
        // The manager runs non-interactively with maximum autonomy so it can shell out to
        // `agentpit` without an approval TTY.
        AutonomyLevel::FullAutonomy
    }
}

/// Whether `b` may act as a workflow manager. Only claude and codex are supported in Phase 1.
pub fn is_supported_manager(b: BackendId) -> bool {
    matches!(b, BackendId::Claude | BackendId::Codex)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exec(backend: BackendId) -> WorkflowManagerExec {
        WorkflowManagerExec {
            backend,
            child_env: vec![
                ("AGENTPIT_WORKFLOW_DEPTH".into(), "1".into()),
                ("AGENTPIT_SELF".into(), "/bin/agentpit".into()),
            ],
            mcp_config_path: None,
        }
    }

    #[test]
    fn claude_spec_bypasses_permissions_with_bash_allowlist() {
        let spec = exec(BackendId::Claude).build_spec("drive the workflow", None);
        assert_eq!(spec.command, "claude");
        assert!(spec.args.iter().any(|a| a == "bypassPermissions"));
        assert!(spec.args.iter().any(|a| a == "stream-json"));
        assert!(spec.args.iter().any(|a| a == "--include-partial-messages"));
        assert_eq!(
            exec(BackendId::Claude).stream_format(),
            StreamFormat::ClaudeJsonl
        );
        assert!(spec.args.iter().any(|a| a == "--allowedTools"));
        assert!(spec.args.iter().any(|a| a == "Bash"));
        // CLI mode must NOT carry any MCP wiring.
        assert!(!spec.args.iter().any(|a| a == "--mcp-config"));
        assert!(!spec.args.iter().any(|a| a == "mcp__agentpit__*"));
        // The prompt is delivered on stdin, NOT as a trailing positional — Claude's variadic
        // `--allowedTools` would otherwise swallow it, leaving `--print` with no input.
        assert_eq!(spec.stdin_input.as_deref(), Some("drive the workflow"));
        assert!(!spec.args.iter().any(|a| a == "drive the workflow"));
        assert!(
            spec.env
                .contains(&("AGENTPIT_WORKFLOW_DEPTH".into(), "1".into()))
        );
    }

    #[test]
    fn claude_mcp_mode_uses_mcp_config_and_tool_allowlist() {
        let mut e = exec(BackendId::Claude);
        e.mcp_config_path = Some(PathBuf::from("/tmp/agentpit-mcp-run-1.json"));
        let spec = e.build_spec("drive the workflow", None);
        assert_eq!(spec.command, "claude");
        assert!(spec.args.iter().any(|a| a == "bypassPermissions"));
        // MCP mode wires --mcp-config <path> + the mcp__agentpit__* tool scope.
        assert!(spec.args.iter().any(|a| a == "--mcp-config"));
        assert!(
            spec.args
                .iter()
                .any(|a| a == "/tmp/agentpit-mcp-run-1.json")
        );
        assert!(spec.args.iter().any(|a| a == "mcp__agentpit__*"));
        // ...and drops the Bash allowlist entirely.
        assert!(!spec.args.iter().any(|a| a == "Bash"));
        // The prompt still goes on stdin.
        assert_eq!(spec.stdin_input.as_deref(), Some("drive the workflow"));
    }

    #[test]
    fn mcp_config_guard_writes_then_removes_file() {
        let guard = McpConfigGuard::write("/usr/local/bin/agentpit", "test-guard-1").unwrap();
        let path = guard.path().to_path_buf();
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("\"mcpServers\""));
        assert!(body.contains("\"agentpit\""));
        assert!(body.contains("/usr/local/bin/agentpit"));
        assert!(body.contains("serve"));
        // The config carries the binary path + server command line; on Unix it must be created
        // owner-only (0o600) so a local user cannot read it during the manager dispatch window.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "MCP config must be owner-only, got {mode:o}");
        }
        drop(guard);
        assert!(!path.exists(), "guard must remove the config on drop");
    }

    #[test]
    fn codex_spec_uses_danger_full_access_and_stdin() {
        let spec = exec(BackendId::Codex).build_spec("drive the workflow", None);
        assert_eq!(spec.command, "codex");
        assert!(spec.args.iter().any(|a| a == "danger-full-access"));
        assert!(spec.args.iter().any(|a| a == "--json"));
        assert_eq!(
            exec(BackendId::Codex).stream_format(),
            StreamFormat::CodexJsonl
        );
        assert_eq!(spec.stdin_input.as_deref(), Some("drive the workflow"));
        assert!(
            spec.env
                .contains(&("AGENTPIT_SELF".into(), "/bin/agentpit".into()))
        );
        assert!(!spec.args.iter().any(|a| a == "--model"));
    }

    #[test]
    fn manager_model_flag_precedes_allowedtools_for_claude_and_dash_for_codex() {
        // claude: --model must land before the variadic --allowedTools.
        let c = exec(BackendId::Claude).build_spec("go", Some("opus"));
        let mi = c.args.iter().position(|a| a == "--model").unwrap();
        let ti = c.args.iter().position(|a| a == "--allowedTools").unwrap();
        assert_eq!(c.args[mi + 1], "opus");
        assert!(mi < ti, "--model must precede --allowedTools");
        // codex: --model before the stdin `-`.
        let x = exec(BackendId::Codex).build_spec("go", Some("gpt-5-codex"));
        let mi = x.args.iter().position(|a| a == "--model").unwrap();
        assert_eq!(x.args[mi + 1], "gpt-5-codex");
        assert_eq!(x.args.last().unwrap(), "-");
    }

    #[test]
    fn only_claude_and_codex_are_supported_managers() {
        assert!(is_supported_manager(BackendId::Claude));
        assert!(is_supported_manager(BackendId::Codex));
        assert!(!is_supported_manager(BackendId::Opencode));
        assert!(!is_supported_manager(BackendId::Antigravity));
        assert!(!is_supported_manager(BackendId::Opencode));
    }

    #[test]
    fn declares_full_autonomy() {
        assert_eq!(
            exec(BackendId::Claude).autonomy(),
            AutonomyLevel::FullAutonomy
        );
        assert_eq!(
            exec(BackendId::Codex).autonomy(),
            AutonomyLevel::FullAutonomy
        );
    }
}
