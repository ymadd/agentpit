use super::{AutonomyLevel, ExecAdapter, ExecSpec, StreamFormat};
use crate::effort::Effort;
use crate::types::BackendId;

pub struct CodexExec;

impl ExecAdapter for CodexExec {
    fn id(&self) -> BackendId {
        BackendId::Codex
    }

    fn build_spec(&self, task: &str, model: Option<&str>, effort: Option<Effort>) -> ExecSpec {
        let mut args = vec![
            "exec".into(),
            "--skip-git-repo-check".into(),
            "--json".into(),
            // `codex exec` sandboxes to READ-ONLY by default, so without this every codex
            // dispatch narrated the change it would have made and wrote nothing ("I couldn't
            // create probe.txt because the workspace is configured as read-only" — verified
            // against codex 0.146.0, 2026-07-31). `workspace-write` is the level that matches
            // the [`AutonomyLevel::FullAutonomy`] this adapter declares: edit and run inside
            // cwd, no approval TTY. Deliberately NOT `danger-full-access` — that is reserved
            // for the workflow manager, which has to shell out to `agentpit` itself.
            "--sandbox".into(),
            "workspace-write".into(),
        ];
        if let Some(m) = model {
            // codex exec: `--model <id>` (also `-m`).
            args.push("--model".into());
            args.push(m.to_string());
        }
        if let Some(e) = effort {
            // codex has no effort *flag*: reasoning effort is a config key, overridden per
            // invocation with `-c key=value` (`codex exec --help`). Its ladder tops out at
            // xhigh, so `max` clamps.
            args.push("-c".into());
            args.push(format!(
                "model_reasoning_effort={}",
                e.clamp_for(BackendId::Codex)
            ));
        }
        args.push("-".into()); // read the prompt from stdin
        ExecSpec {
            command: "codex".into(),
            args,
            env: Vec::new(),
            stdin_input: Some(task.to_string()),
        }
    }

    fn stream_format(&self) -> StreamFormat {
        StreamFormat::CodexJsonl
    }

    fn autonomy(&self) -> AutonomyLevel {
        // `codex exec` is the inherently non-interactive subcommand: it applies its work
        // without an approval TTY, so it carries the same full-autonomy posture.
        AutonomyLevel::FullAutonomy
    }

    fn supports_resume(&self) -> bool {
        true
    }

    fn build_continuation_spec(
        &self,
        task: &str,
        model: Option<&str>,
        effort: Option<Effort>,
        backend_ref: &str,
    ) -> Option<ExecSpec> {
        // `codex exec resume <thread_id> …` continues the thread and re-emits the same
        // thread_id on `thread.started` (verified against the real CLI 2026-08-08). The
        // resume subcommand has no `--sandbox`/`--model` flags, so both ride the documented
        // `-c` config overrides (raw-literal TOML values); the sandbox override keeps the
        // adapter's declared [`AutonomyLevel::FullAutonomy`] honest rather than trusting
        // whatever the original session ran with.
        let mut args = vec![
            "exec".into(),
            "resume".into(),
            backend_ref.to_string(),
            "--skip-git-repo-check".into(),
            "--json".into(),
            "-c".into(),
            "sandbox_mode=workspace-write".into(),
        ];
        if let Some(m) = model {
            args.push("-c".into());
            args.push(format!("model={m}"));
        }
        if let Some(e) = effort {
            args.push("-c".into());
            args.push(format!(
                "model_reasoning_effort={}",
                e.clamp_for(BackendId::Codex)
            ));
        }
        args.push("-".into()); // read the prompt from stdin
        Some(ExecSpec {
            command: "codex".into(),
            args,
            env: Vec::new(),
            stdin_input: Some(task.to_string()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_task_via_stdin() {
        let spec = CodexExec.build_spec("summarize file", None, None);
        assert_eq!(spec.command, "codex");
        assert_eq!(
            spec.args,
            vec![
                "exec",
                "--skip-git-repo-check",
                "--json",
                "--sandbox",
                "workspace-write",
                "-"
            ]
        );
        assert_eq!(CodexExec.stream_format(), StreamFormat::CodexJsonl);
        assert_eq!(spec.stdin_input.as_deref(), Some("summarize file"));
    }

    #[test]
    fn model_inserts_model_flag_before_the_stdin_dash() {
        let spec = CodexExec.build_spec("x", Some("gpt-5-codex"), None);
        assert_eq!(
            spec.args,
            vec![
                "exec",
                "--skip-git-repo-check",
                "--json",
                "--sandbox",
                "workspace-write",
                "--model",
                "gpt-5-codex",
                "-"
            ]
        );
        assert_eq!(spec.stdin_input.as_deref(), Some("x")); // task still on stdin
    }

    #[test]
    fn effort_becomes_a_config_override_clamped_at_xhigh() {
        let spec = CodexExec.build_spec("x", None, Some(Effort::High));
        let i = spec.args.iter().position(|a| a == "-c").unwrap();
        assert_eq!(spec.args[i + 1], "model_reasoning_effort=high");
        assert_eq!(spec.args.last().unwrap(), "-"); // stdin dash stays last
        // `max` has no codex rung; it clamps down to xhigh rather than being dropped.
        let maxed = CodexExec.build_spec("x", None, Some(Effort::Max));
        assert!(
            maxed
                .args
                .iter()
                .any(|a| a == "model_reasoning_effort=xhigh")
        );
    }

    /// The declared autonomy has to be backed by a sandbox that can actually write. Without
    /// this the adapter claimed full autonomy while codex silently refused every edit.
    #[test]
    fn declares_full_autonomy_and_carries_a_writable_sandbox() {
        assert_eq!(CodexExec.autonomy(), AutonomyLevel::FullAutonomy);
        let spec = CodexExec.build_spec("x", None, None);
        assert_eq!(spec.args.first().map(String::as_str), Some("exec"));
        let i = spec.args.iter().position(|a| a == "--sandbox").unwrap();
        assert_eq!(spec.args[i + 1], "workspace-write");
        // Full shell access stays reserved for the workflow manager.
        assert!(!spec.args.iter().any(|a| a == "danger-full-access"));
    }

    /// `codex exec resume` has no `--sandbox`/`--model` flags (verified 2026-08-08), so the
    /// continuation spec must carry both as `-c` config overrides, keep the thread id as the
    /// positional right after `resume`, and still read the prompt from stdin.
    #[test]
    fn continuation_spec_uses_resume_subcommand_with_config_overrides() {
        assert!(CodexExec.supports_resume());
        let spec = CodexExec
            .build_continuation_spec(
                "follow up",
                Some("gpt-5-codex"),
                Some(Effort::High),
                "019fe072-tid",
            )
            .unwrap();
        assert_eq!(spec.args[0], "exec");
        assert_eq!(spec.args[1], "resume");
        assert_eq!(spec.args[2], "019fe072-tid");
        assert!(spec.args.iter().any(|a| a == "--json"));
        assert!(!spec.args.iter().any(|a| a == "--sandbox"));
        assert!(
            spec.args
                .iter()
                .any(|a| a == "sandbox_mode=workspace-write")
        );
        assert!(spec.args.iter().any(|a| a == "model=gpt-5-codex"));
        assert!(spec.args.iter().any(|a| a == "model_reasoning_effort=high"));
        assert_eq!(spec.args.last().unwrap(), "-");
        assert_eq!(spec.stdin_input.as_deref(), Some("follow up"));
    }
}
