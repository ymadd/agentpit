use super::{ExecAdapter, ExecSpec};
use crate::types::BackendId;

pub struct AntigravityExec;

impl ExecAdapter for AntigravityExec {
    fn id(&self) -> BackendId {
        BackendId::Antigravity
    }

    fn build_spec(&self, task: &str) -> ExecSpec {
        ExecSpec {
            command: "agy".into(),
            args: vec![
                "--yolo".into(),
                "-p".into(),
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
    fn passes_task_as_prompt_arg() {
        let spec = AntigravityExec.build_spec("hello world");
        assert_eq!(spec.command, "agy");
        assert!(spec.args.iter().any(|a| a == "--yolo"));
        assert!(spec.args.iter().any(|a| a == "-p"));
        assert_eq!(spec.args.last().unwrap(), "hello world");
        assert!(spec.stdin_input.is_none());
    }
}
