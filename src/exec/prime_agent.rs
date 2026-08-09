use super::{AutonomyLevel, ExecAdapter, ExecSpec, StreamFormat};
use crate::effort::Effort;
use crate::types::BackendId;

/// Prime Agent (`prime-agent`) — Prime Intellect's RLM-native coding/research harness.
///
/// Dispatched in its JSON event-stream mode (`--mode json`), which writes one event per line to
/// stdout and exits with the run's status. That is the batch-friendly mode its own docs
/// recommend over ACP for "dump every event and give me an exit code", and it is what lets
/// agentpit stream live text plus tool progress while keeping the collected answer JSONL-free
/// (see [`StreamFormat::PrimeAgentJsonl`]).
///
/// Verified against prime-agent 0.7.1 (2026-08-08).
pub struct PrimeAgentExec;

impl ExecAdapter for PrimeAgentExec {
    fn id(&self) -> BackendId {
        BackendId::PrimeAgent
    }

    fn build_spec(&self, task: &str, model: Option<&str>, effort: Option<Effort>) -> ExecSpec {
        let mut args = vec!["--mode".into(), "json".into()];
        if let Some(m) = model {
            // prime-agent: `--model` takes either a bare id or the canonical
            // `provider/model` selector, resolved by splitting on the FIRST slash. The
            // dashboard catalog stores the canonical form (a bare Prime Inference id like
            // "anthropic/claude-opus-5" would route to the Anthropic provider instead), and
            // agentpit passes whatever it was given through verbatim — never splitting or
            // reassembling it here.
            args.push("--model".into());
            args.push(m.to_string());
        }
        if let Some(e) = effort {
            // prime-agent calls the rung `--thinking` and accepts
            // off|minimal|low|medium|high|xhigh|max — a superset of the canonical ladder, so
            // nothing clamps away here. An unknown value is only WARNED about, never fatal,
            // which is exactly why the value is taken from `clamp_for` rather than free text.
            args.push("--thinking".into());
            args.push(e.clamp_for(BackendId::PrimeAgent).as_str().into());
        }
        // `--` makes every following argument a message. Without it a task that happens to
        // start with a dash ("--check the parser") would be parsed as a flag and rejected.
        args.push("--".into());
        args.push(task.to_string());
        ExecSpec {
            command: "prime-agent".into(),
            args,
            env: Vec::new(),
            stdin_input: None,
        }
    }

    fn stream_format(&self) -> StreamFormat {
        StreamFormat::PrimeAgentJsonl
    }

    fn autonomy(&self) -> AutonomyLevel {
        // prime-agent has no approval gate to bypass: its IPython tool executes model-generated
        // Python and shell with the worker's own OS permissions, and non-interactive modes have
        // no UI to confirm through. So there is no flag here to audit — the posture is inherent,
        // and declaring it keeps that visible next to the other backends instead of implied.
        AutonomyLevel::FullAutonomy
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runs_in_json_mode_with_the_task_after_the_separator() {
        let spec = PrimeAgentExec.build_spec("write a haiku", None, None);
        assert_eq!(spec.command, "prime-agent");
        assert_eq!(spec.args, ["--mode", "json", "--", "write a haiku"]);
        assert_eq!(
            PrimeAgentExec.stream_format(),
            StreamFormat::PrimeAgentJsonl
        );
        assert!(spec.stdin_input.is_none());
        // No pins → no flags at all (byte-identical to an unpinned dispatch).
        assert!(!spec.args.iter().any(|a| a == "--model"));
        assert!(!spec.args.iter().any(|a| a == "--thinking"));
    }

    #[test]
    fn model_is_passed_through_verbatim_slashes_and_all() {
        let spec = PrimeAgentExec.build_spec("x", Some("anthropic/claude-opus-5"), None);
        let i = spec.args.iter().position(|a| a == "--model").unwrap();
        assert_eq!(spec.args[i + 1], "anthropic/claude-opus-5");
        assert_eq!(spec.args.last().unwrap(), "x");
    }

    #[test]
    fn effort_becomes_thinking_and_carries_the_whole_ladder() {
        for e in Effort::ALL {
            let spec = PrimeAgentExec.build_spec("x", None, Some(*e));
            let i = spec.args.iter().position(|a| a == "--thinking").unwrap();
            // prime-agent accepts every canonical rung, so nothing is clamped on the way out.
            assert_eq!(spec.args[i + 1], e.as_str());
            assert_eq!(spec.args.last().unwrap(), "x");
        }
    }

    /// Regression guard for the flag-eats-the-task shape: the task is always the LAST argument
    /// and is always preceded by `--`, whichever pins are set.
    #[test]
    fn the_separator_always_immediately_precedes_the_task() {
        for (model, effort) in [
            (None, None),
            (Some("claude-opus-5"), None),
            (None, Some(Effort::High)),
            (Some("claude-opus-5"), Some(Effort::Max)),
        ] {
            let spec = PrimeAgentExec.build_spec("--check the parser", model, effort);
            let n = spec.args.len();
            assert_eq!(spec.args[n - 2], "--", "got: {:?}", spec.args);
            assert_eq!(spec.args[n - 1], "--check the parser");
        }
    }

    #[test]
    fn declares_full_autonomy() {
        assert_eq!(PrimeAgentExec.autonomy(), AutonomyLevel::FullAutonomy);
    }
}
