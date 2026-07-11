use super::{AutonomyLevel, ExecAdapter, ExecSpec};
use crate::types::BackendId;

pub struct GeminiExec;

impl ExecAdapter for GeminiExec {
    fn id(&self) -> BackendId {
        BackendId::Gemini
    }

    fn build_spec(&self, task: &str, model: Option<&str>) -> ExecSpec {
        let mut args = vec![
            "--yolo".into(),
            "--skip-trust".into(),
            "--output-format".into(),
            "text".into(),
        ];
        if let Some(m) = model {
            // gemini CLI: `-m <model>` (also `--model`).
            args.push("-m".into());
            args.push(m.to_string());
        }
        args.push("-p".into());
        args.push(task.to_string());
        ExecSpec {
            command: "gemini".into(),
            args,
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
        let spec = GeminiExec.build_spec("hello world", None);
        assert_eq!(spec.command, "gemini");
        assert!(spec.args.iter().any(|a| a == "--yolo"));
        assert!(spec.args.iter().any(|a| a == "--skip-trust"));
        assert_eq!(spec.args.last().unwrap(), "hello world");
        assert!(spec.stdin_input.is_none());
        assert!(!spec.args.iter().any(|a| a == "-m"));
    }

    #[test]
    fn model_adds_dash_m_before_the_prompt() {
        let spec = GeminiExec.build_spec("x", Some("gemini-3-pro"));
        let i = spec.args.iter().position(|a| a == "-m").unwrap();
        assert_eq!(spec.args[i + 1], "gemini-3-pro");
        assert_eq!(spec.args.last().unwrap(), "x"); // -p <task> stays last
    }

    #[test]
    fn declares_full_autonomy_and_carries_yolo() {
        assert_eq!(GeminiExec.autonomy(), AutonomyLevel::FullAutonomy);
        let spec = GeminiExec.build_spec("x", None);
        assert!(spec.args.iter().any(|a| a == "--yolo"));
    }
}
