use super::{ExecAdapter, ExecSpec};
use crate::types::BackendId;

pub struct ClaudeExec;

impl ExecAdapter for ClaudeExec {
    fn id(&self) -> BackendId {
        BackendId::Claude
    }

    fn build_spec(&self, task: &str) -> ExecSpec {
        ExecSpec {
            command: "claude".into(),
            args: vec![
                "--print".into(),
                "--output-format".into(),
                "text".into(),
                "--permission-mode".into(),
                "acceptEdits".into(),
                task.to_string(),
            ],
            env: Vec::new(),
            stdin_input: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_print_and_accept_edits_flags() {
        let spec = ClaudeExec.build_spec("write a haiku");
        assert_eq!(spec.command, "claude");
        assert!(spec.args.iter().any(|a| a == "--print"));
        assert!(spec.args.iter().any(|a| a == "acceptEdits"));
        assert_eq!(spec.args.last().unwrap(), "write a haiku");
        assert!(spec.stdin_input.is_none());
    }
}
