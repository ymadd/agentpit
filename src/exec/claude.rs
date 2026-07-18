use super::{AutonomyLevel, ExecAdapter, ExecSpec, StreamFormat};
use crate::types::BackendId;

pub struct ClaudeExec;

impl ExecAdapter for ClaudeExec {
    fn id(&self) -> BackendId {
        BackendId::Claude
    }

    fn build_spec(&self, task: &str, model: Option<&str>) -> ExecSpec {
        let mut args = vec![
            "--print".into(),
            "--output-format".into(),
            "stream-json".into(),
            "--include-partial-messages".into(),
            "--verbose".into(),
            "--permission-mode".into(),
            "acceptEdits".into(),
        ];
        if let Some(m) = model {
            // claude CLI: `--model <alias|id>` (e.g. opus, sonnet, claude-opus-4-8).
            args.push("--model".into());
            args.push(m.to_string());
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
        // `--permission-mode acceptEdits` auto-accepts edits without prompting.
        AutonomyLevel::FullAutonomy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uses_print_and_accept_edits_flags() {
        let spec = ClaudeExec.build_spec("write a haiku", None);
        assert_eq!(spec.command, "claude");
        assert!(spec.args.iter().any(|a| a == "--print"));
        assert!(spec.args.iter().any(|a| a == "stream-json"));
        assert!(spec.args.iter().any(|a| a == "--include-partial-messages"));
        assert!(spec.args.iter().any(|a| a == "--verbose"));
        assert!(spec.args.iter().any(|a| a == "acceptEdits"));
        assert_eq!(ClaudeExec.stream_format(), StreamFormat::ClaudeJsonl);
        assert_eq!(spec.args.last().unwrap(), "write a haiku");
        assert!(spec.stdin_input.is_none());
        // No model → no --model flag (byte-identical to the pre-model spec).
        assert!(!spec.args.iter().any(|a| a == "--model"));
    }

    #[test]
    fn model_adds_model_flag_before_the_task() {
        let spec = ClaudeExec.build_spec("x", Some("opus"));
        let i = spec.args.iter().position(|a| a == "--model").unwrap();
        assert_eq!(spec.args[i + 1], "opus");
        assert_eq!(spec.args.last().unwrap(), "x"); // task stays last
    }

    #[test]
    fn declares_full_autonomy_and_accepts_edits() {
        assert_eq!(ClaudeExec.autonomy(), AutonomyLevel::FullAutonomy);
        let spec = ClaudeExec.build_spec("x", None);
        assert!(spec.args.iter().any(|a| a == "acceptEdits"));
    }
}
