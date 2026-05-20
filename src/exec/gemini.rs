use super::{ExecAdapter, ExecSpec};
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
}
