use super::autonomy::claude_permission_mode;
use super::{AutonomyLevel, ExecAdapter, ExecSpec, StreamFormat};
use crate::effort::Effort;
use crate::types::BackendId;

pub struct ClaudeExec;

impl ExecAdapter for ClaudeExec {
    fn id(&self) -> BackendId {
        BackendId::Claude
    }

    fn build_spec(&self, task: &str, model: Option<&str>, effort: Option<Effort>) -> ExecSpec {
        let mut args = vec![
            "--print".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--include-partial-messages".into(),
            "--verbose".into(),
            "--permission-mode".into(),
            // Chosen from the model, because auto mode's classifier is not on every one.
            claude_permission_mode(model).into(),
        ];
        if let Some(m) = model {
            // claude CLI: `--model <alias|id>` (e.g. opus, sonnet, claude-opus-4-8).
            args.push("--model".into());
            args.push(m.to_string());
        }
        if let Some(e) = effort {
            // claude CLI: `--effort <level>` (low, medium, high, xhigh, max) — the whole
            // canonical ladder, so nothing clamps away here.
            args.push("--effort".into());
            args.push(e.clamp_for(BackendId::Claude).as_str().into());
        }
        args.push(task.to_string());
        ExecSpec {
            command: "claude".into(),
            args,
            env: Vec::new(),
            stdin_input: None,
        }
    }

    fn stream_format(&self) -> StreamFormat {
        StreamFormat::ClaudeJsonl
    }

    fn autonomy(&self) -> AutonomyLevel {
        // `--permission-mode auto` acts without a prompt; a classifier reviews each action
        // rather than a human. `acceptEdits`, the fallback for a model with no classifier,
        // is narrower but still needs no TTY. Either way nothing here waits on approval.
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
        // `claude --print --resume <session_id> <task>` continues the session in place and
        // keeps the SAME session_id on the stream (verified against the real CLI 2026-08-08).
        let mut spec = self.build_spec(task, model, effort);
        let task_arg = spec.args.pop().expect("build_spec always pushes the task");
        spec.args.push("--resume".into());
        spec.args.push(backend_ref.to_string());
        spec.args.push(task_arg);
        Some(spec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The value that follows `--permission-mode`, so a test cannot pass on a bare word that
    /// happens to appear somewhere else in the argv (a model named `auto`, say).
    fn permission_mode(spec: &ExecSpec) -> &str {
        let i = spec
            .args
            .iter()
            .position(|a| a == "--permission-mode")
            .expect("every claude spec carries a permission mode");
        &spec.args[i + 1]
    }

    #[test]
    fn uses_print_and_auto_permission_flags() {
        let spec = ClaudeExec.build_spec("write a haiku", None, None);
        assert_eq!(spec.command, "claude");
        assert!(spec.args.iter().any(|a| a == "--print"));
        assert!(spec.args.iter().any(|a| a == "stream-json"));
        assert!(spec.args.iter().any(|a| a == "--include-partial-messages"));
        assert!(spec.args.iter().any(|a| a == "--verbose"));
        // The MODEL picks the mode, never the task — a task that says "haiku" is not a run
        // on Haiku, and reading the wrong one would quietly narrow every such dispatch.
        assert_eq!(permission_mode(&spec), "auto");
        assert_eq!(ClaudeExec.stream_format(), StreamFormat::ClaudeJsonl);
        assert_eq!(spec.args.last().unwrap(), "write a haiku");
        assert!(spec.stdin_input.is_none());
        // No model → no --model flag (byte-identical to the pre-model spec).
        assert!(!spec.args.iter().any(|a| a == "--model"));
        assert!(!spec.args.iter().any(|a| a == "--effort"));
    }

    #[test]
    fn model_adds_model_flag_before_the_task() {
        let spec = ClaudeExec.build_spec("x", Some("opus"), None);
        let i = spec.args.iter().position(|a| a == "--model").unwrap();
        assert_eq!(spec.args[i + 1], "opus");
        assert_eq!(spec.args.last().unwrap(), "x"); // task stays last
    }

    #[test]
    fn effort_adds_effort_flag_and_carries_the_whole_ladder() {
        for e in Effort::ALL {
            let spec = ClaudeExec.build_spec("x", None, Some(*e));
            let i = spec.args.iter().position(|a| a == "--effort").unwrap();
            // claude accepts every rung, so nothing is clamped on the way out.
            assert_eq!(spec.args[i + 1], e.as_str());
            assert_eq!(spec.args.last().unwrap(), "x"); // task stays last
        }
    }

    #[test]
    fn declares_full_autonomy_and_needs_no_approval_tty() {
        assert_eq!(ClaudeExec.autonomy(), AutonomyLevel::FullAutonomy);
        // The declared autonomy has to be backed by a mode that acts without a prompt —
        // the same obligation `codex.rs` has toward its sandbox. `auto` is classifier-
        // reviewed rather than unchecked, and a model with no classifier still gets a
        // promptless mode instead of one that would stall on a TTY that does not exist.
        let current = ClaudeExec.build_spec("x", None, None);
        assert_eq!(permission_mode(&current), "auto");
        let pinned_old = ClaudeExec.build_spec("x", Some("claude-opus-4-5"), None);
        assert_eq!(permission_mode(&pinned_old), "acceptEdits");
        // `default` and `plan` would prompt, and `dontAsk` would deny — none may appear.
        for spec in [
            ClaudeExec.build_spec("x", None, None),
            ClaudeExec.build_spec("x", Some("haiku"), None),
        ] {
            assert!(
                matches!(permission_mode(&spec), "auto" | "acceptEdits"),
                "{} would stall or deny without a human",
                permission_mode(&spec)
            );
        }
    }

    #[test]
    fn continuation_spec_inserts_resume_before_the_task() {
        assert!(ClaudeExec.supports_resume());
        let spec = ClaudeExec
            .build_continuation_spec("follow up", Some("opus"), None, "0c1ff28e-sid")
            .unwrap();
        let i = spec.args.iter().position(|a| a == "--resume").unwrap();
        assert_eq!(spec.args[i + 1], "0c1ff28e-sid");
        // The task stays the final positional argument, after every flag.
        assert_eq!(spec.args.last().unwrap(), "follow up");
        // Everything else matches the fresh spec (same output format, permission mode, model).
        assert!(spec.args.iter().any(|a| a == "stream-json"));
        assert_eq!(permission_mode(&spec), "auto");
        assert!(spec.args.iter().any(|a| a == "opus"));
    }
}
