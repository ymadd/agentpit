use super::{AutonomyLevel, ExecAdapter, ExecSpec};
use crate::types::BackendId;

pub struct GeminiExec;

impl ExecAdapter for GeminiExec {
    fn id(&self) -> BackendId {
        BackendId::Gemini
    }

    fn build_spec(&self, task: &str) -> ExecSpec {
        ExecSpec {
            command: "gemini".into(),
            args: vec![
                "--yolo".into(),
                "--skip-trust".into(),
                "--output-format".into(),
                "text".into(),
                "-p".into(),
                task.to_string(),
            ],
            env: Vec::new(),
            stdin_input: None,
        }
    }

    fn autonomy(&self) -> AutonomyLevel {
        // `--yolo --skip-trust` runs every tool call without confirmation.
        AutonomyLevel::FullAutonomy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_task_as_prompt_arg() {
        let spec = GeminiExec.build_spec("hello world");
        assert_eq!(spec.command, "gemini");
        assert!(spec.args.iter().any(|a| a == "--yolo"));
        assert!(spec.args.iter().any(|a| a == "--skip-trust"));
        assert_eq!(spec.args.last().unwrap(), "hello world");
        assert!(spec.stdin_input.is_none());
    }

    #[test]
    fn declares_full_autonomy_and_carries_yolo() {
        assert_eq!(GeminiExec.autonomy(), AutonomyLevel::FullAutonomy);
        let spec = GeminiExec.build_spec("x");
        assert!(spec.args.iter().any(|a| a == "--yolo"));
    }
}
