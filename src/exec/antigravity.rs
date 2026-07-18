use super::{AutonomyLevel, ExecAdapter, ExecSpec};
use crate::types::BackendId;

pub struct AntigravityExec;

impl ExecAdapter for AntigravityExec {
    fn id(&self) -> BackendId {
        BackendId::Antigravity
    }

    fn build_spec(&self, task: &str, model: Option<&str>) -> ExecSpec {
        let mut args = vec!["--dangerously-skip-permissions".into(), "--print".into()];
        if let Some(m) = model {
            // agy (Gemini-CLI-derived): `--model <id>`.
            args.push("--model".into());
            args.push(m.to_string());
        }
        args.push(task.to_string());
        ExecSpec {
            command: "agy".into(),
            args,
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
        let spec = AntigravityExec.build_spec("hello world", None);
        assert_eq!(spec.command, "agy");
        assert!(
            spec.args
                .iter()
                .any(|a| a == "--dangerously-skip-permissions")
        );
        assert!(spec.args.iter().any(|a| a == "--print"));
        assert_eq!(spec.args.last().unwrap(), "hello world");
        assert!(spec.stdin_input.is_none());
        assert!(!spec.args.iter().any(|a| a == "--model"));
    }

    #[test]
    fn model_adds_model_flag_before_the_task() {
        let spec = AntigravityExec.build_spec("x", Some("gemini-3-pro"));
        let i = spec.args.iter().position(|a| a == "--model").unwrap();
        assert_eq!(spec.args[i + 1], "gemini-3-pro");
        assert_eq!(spec.args.last().unwrap(), "x");
    }

    #[test]
    fn declares_full_autonomy_and_carries_the_bypass_flag() {
        assert_eq!(AntigravityExec.autonomy(), AutonomyLevel::FullAutonomy);
        let spec = AntigravityExec.build_spec("x", None);
        assert!(
            spec.args
                .iter()
                .any(|a| a == "--dangerously-skip-permissions")
        );
    }
}
