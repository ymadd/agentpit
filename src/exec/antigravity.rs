use super::{AutonomyLevel, ExecAdapter, ExecSpec};
use crate::effort::Effort;
use crate::types::BackendId;

pub struct AntigravityExec;

impl ExecAdapter for AntigravityExec {
    fn id(&self) -> BackendId {
        BackendId::Antigravity
    }

    fn build_spec(&self, task: &str, model: Option<&str>, effort: Option<Effort>) -> ExecSpec {
        // `--print` is an ALIAS FOR `--prompt` and takes the prompt as its value, so it must be
        // the LAST flag, immediately before the task. Any flag emitted between the two is eaten
        // as the prompt: `agy --print --model gemini-3-flash "Reply with exactly: OK"` answers
        // "I am currently running on Gemini 3.6 Flash" instead of "OK" (verified against agy
        // 1.1.8, 2026-07-31). Every other flag therefore goes first.
        let mut args = vec!["--dangerously-skip-permissions".into()];
        if let Some(m) = model {
            // agy (Gemini-CLI-derived): `--model <id>`.
            args.push("--model".into());
            args.push(m.to_string());
        }
        // agy: `--effort <low|medium|high>` — the shortest ladder of the four, so both xhigh and
        // max clamp to high. But agy's model ids BUNDLE the effort ("gemini-3.6-flash-high"), and
        // it rejects a separate `--effort` for such a model: `--effort is not supported for model
        // "Gemini 3.5 Flash (High)"` (agy 1.1.8, 2026-07-31), which would fail the whole dispatch.
        // So the pinned model wins and the rung is dropped with a visible note — for agy, choosing
        // the model IS choosing the effort.
        match (effort, model) {
            (Some(e), None) => {
                args.push("--effort".into());
                args.push(e.clamp_for(BackendId::Antigravity).as_str().into());
            }
            (Some(e), Some(m)) => eprintln!(
                "warning: [antigravity] ignoring --effort {e}: agy binds effort to the model id, \
                 and model \"{m}\" already carries its own level. Pick a model such as \
                 gemini-3.6-flash-{} to change it (see `agy models`).",
                e.clamp_for(BackendId::Antigravity)
            ),
            (None, _) => {}
        }
        args.push("--print".into());
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
        let spec = AntigravityExec.build_spec("hello world", None, None);
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
        assert!(!spec.args.iter().any(|a| a == "--effort"));
    }

    #[test]
    fn model_adds_model_flag_before_the_task() {
        let spec = AntigravityExec.build_spec("x", Some("gemini-3-pro"), None);
        let i = spec.args.iter().position(|a| a == "--model").unwrap();
        assert_eq!(spec.args[i + 1], "gemini-3-pro");
        assert_eq!(spec.args.last().unwrap(), "x");
    }

    #[test]
    fn effort_clamps_to_agys_three_rung_ladder() {
        for (asked, expected) in [
            (Effort::Low, "low"),
            (Effort::Medium, "medium"),
            (Effort::High, "high"),
            (Effort::XHigh, "high"),
            (Effort::Max, "high"),
        ] {
            let spec = AntigravityExec.build_spec("x", None, Some(asked));
            let i = spec.args.iter().position(|a| a == "--effort").unwrap();
            assert_eq!(spec.args[i + 1], expected, "asked for {asked}");
            assert_eq!(spec.args.last().unwrap(), "x");
        }
    }

    /// A pinned model already carries agy's effort level, and agy errors out when both are
    /// present — so the model wins and no `--effort` is emitted.
    #[test]
    fn a_pinned_model_suppresses_the_effort_flag() {
        let spec =
            AntigravityExec.build_spec("x", Some("gemini-3.6-flash-high"), Some(Effort::Low));
        assert!(
            !spec.args.iter().any(|a| a == "--effort"),
            "{:?}",
            spec.args
        );
        let i = spec.args.iter().position(|a| a == "--model").unwrap();
        assert_eq!(spec.args[i + 1], "gemini-3.6-flash-high");
    }

    /// Regression guard for the `--print`-eats-the-next-token bug: `--print` must be the last
    /// flag, with only the task after it, no matter which optional flags are set.
    #[test]
    fn print_is_always_the_last_flag_before_the_task() {
        for (model, effort) in [
            (None, None),
            (Some("gemini-3-pro"), None),
            (None, Some(Effort::High)),
            (Some("gemini-3-pro"), Some(Effort::Max)),
        ] {
            let spec = AntigravityExec.build_spec("do the thing", model, effort);
            let n = spec.args.len();
            assert_eq!(spec.args[n - 2], "--print", "got: {:?}", spec.args);
            assert_eq!(spec.args[n - 1], "do the thing");
        }
    }

    #[test]
    fn declares_full_autonomy_and_carries_the_bypass_flag() {
        assert_eq!(AntigravityExec.autonomy(), AutonomyLevel::FullAutonomy);
        let spec = AntigravityExec.build_spec("x", None, None);
        assert!(
            spec.args
                .iter()
                .any(|a| a == "--dangerously-skip-permissions")
        );
    }
}
