use super::{ExecAdapter, ExecSpec};
use crate::types::BackendId;

pub struct CodexExec;

impl ExecAdapter for CodexExec {
    fn id(&self) -> BackendId {
        BackendId::Codex
    }

    fn build_spec(&self, task: &str) -> ExecSpec {
        ExecSpec {
            command: "codex".into(),
            args: vec![
                "exec".into(),
                "--skip-git-repo-check".into(),
                "-".into(),
            ],
            env: Vec::new(),
            stdin_input: Some(task.to_string()),
        }
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
}
