use super::{AutonomyLevel, ExecAdapter, ExecSpec};
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
                "--dangerously-skip-permissions".into(),
                "--print".into(),
                task.to_string(),
            ],
            env: Vec::new(),
            stdin_input: None,
        }
    }

    fn autonomy(&self) -> AutonomyLevel {
        // `--dangerously-skip-permissions` bypasses every approval gate.
        AutonomyLevel::FullAutonomy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_task_as_prompt_arg() {
        let spec = AntigravityExec.build_spec("hello world");
        assert_eq!(spec.command, "agy");
        assert!(
            spec.args
                .iter()
                .any(|a| a == "--dangerously-skip-permissions")
        );
        assert!(spec.args.iter().any(|a| a == "--print"));
        assert_eq!(spec.args.last().unwrap(), "hello world");
        assert!(spec.stdin_input.is_none());
    }

    #[test]
    fn declares_full_autonomy_and_carries_the_bypass_flag() {
        assert_eq!(AntigravityExec.autonomy(), AutonomyLevel::FullAutonomy);
        let spec = AntigravityExec.build_spec("x");
        assert!(
            spec.args
                .iter()
                .any(|a| a == "--dangerously-skip-permissions")
        );
    }
}
