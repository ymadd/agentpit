use super::{AutonomyLevel, ExecAdapter, ExecSpec};
use crate::types::BackendId;

pub struct CodexExec;

impl ExecAdapter for CodexExec {
    fn id(&self) -> BackendId {
        BackendId::Codex
    }

    fn build_spec(&self, task: &str) -> ExecSpec {
        ExecSpec {
            command: "codex".into(),
            args: vec!["exec".into(), "--skip-git-repo-check".into(), "-".into()],
            env: Vec::new(),
            stdin_input: Some(task.to_string()),
        }
    }

    fn autonomy(&self) -> AutonomyLevel {
        // `codex exec` is the inherently non-interactive subcommand: it applies its work
        // without an approval TTY, so it carries the same full-autonomy posture.
        AutonomyLevel::FullAutonomy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_task_via_stdin() {
        let spec = CodexExec.build_spec("summarize file");
        assert_eq!(spec.command, "codex");
        assert_eq!(spec.args, vec!["exec", "--skip-git-repo-check", "-"]);
        assert_eq!(spec.stdin_input.as_deref(), Some("summarize file"));
    }

    #[test]
    fn declares_full_autonomy_via_exec_subcommand() {
        assert_eq!(CodexExec.autonomy(), AutonomyLevel::FullAutonomy);
        let spec = CodexExec.build_spec("x");
        assert_eq!(spec.args.first().map(String::as_str), Some("exec"));
    }
}
