//! `agentpit refute-bench` — the design §5.1 go/no-go gate for ④ refute itself: does a live
//! critique→defense pass actually recover a stuck candidate, or merely hold it steady?
//!
//! Runs the small MVP probe set ([`refute_probe_tasks`]) against one critic/defender pair, grades
//! each probe's `stuck` candidate offline (the "before" half) and the defense leg's revised
//! candidate through the same grader (the "after" half), and prints a PASS/FAIL verdict per probe
//! plus the overall gate. Exits non-zero when the gate is red, so it can be wired into CI once the
//! boundary in design §5.1 needs enforcing — for now it is a manually-run measurement, not a gate
//! anything else depends on.

use anyhow::{Result, bail};
use console::style;
use tokio_util::sync::CancellationToken;

use crate::events::{LegStatus, RunKind, RunLogger};
use crate::profile::bench::{
    DELTA_PASS_MARGIN, GradeOutcome, RefuteProbeResult, gate_passes, refute_probe_tasks,
    run_refute_bench,
};
use crate::types::BackendId;
use crate::workflow::converse::resolve_pair;

pub async fn run(
    critic: Option<BackendId>,
    defender: Option<BackendId>,
    cwd: Option<String>,
) -> Result<()> {
    let ctx = super::load_context()?;
    let available = ctx.regs.available();
    let preferred = ctx
        .loaded
        .config
        .ensemble
        .adversarial_review_members
        .clone();
    let (critic, defender) = resolve_pair(critic, defender, &available, &preferred)?;

    let cwd = super::resolve_cwd(cwd)?;
    let cancel = CancellationToken::new();
    super::install_ctrlc_cancel(cancel.clone());

    let tasks = refute_probe_tasks();
    eprintln!(
        "[refute-bench] critic={critic} defender={defender} — {} probe(s)",
        tasks.len()
    );
    if critic == defender {
        eprintln!(
            "[refute-bench] only one backend available; the same backend critiques and defends, \
             which weakens the result."
        );
    }

    let members = if critic == defender {
        vec![critic]
    } else {
        vec![critic, defender]
    };
    let logger = RunLogger::start(RunKind::Bench, &members, &cwd);
    logger.member_started(critic, false, None, None);
    if defender != critic {
        logger.member_started(defender, false, None, None);
    }

    let results = run_refute_bench(&tasks, critic, defender, &cwd, &ctx.regs, cancel).await;
    let passed = gate_passes(&results);

    logger.member_finished(critic, false, LegStatus::Ok, 0, None, None);
    if defender != critic {
        logger.member_finished(defender, false, LegStatus::Ok, 0, None, None);
    }
    logger.finished(if passed {
        LegStatus::Ok
    } else {
        LegStatus::Error
    });

    print!("{}", render_results(&results, DELTA_PASS_MARGIN));
    if passed {
        println!("{}", style("GATE: PASS").green().bold());
        Ok(())
    } else {
        println!("{}", style("GATE: FAIL").red().bold());
        bail!("refute-bench gate did not pass — see probe results above");
    }
}

/// Render the per-probe before/after/delta table. Pure: builds and returns a fresh `String`.
fn render_results(results: &[RefuteProbeResult], margin: f64) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "(pass margin: delta >= {margin:.2})");
    for r in results {
        let before = render_outcome(r.before);
        let after = render_outcome(r.after);
        let delta = match r.delta() {
            Some(d) => format!("{d:+.2}"),
            None => "n/a".to_string(),
        };
        let verdict = if r.passes() {
            style("PASS").green().to_string()
        } else {
            style("FAIL").red().to_string()
        };
        let note = match (r.critique_ok, r.defense_ok) {
            (true, true) => String::new(),
            (false, _) => " (critique leg failed)".to_string(),
            (true, false) => " (defense leg failed)".to_string(),
        };
        let _ = writeln!(
            out,
            "  {:<32} before={:<6} after={:<6} delta={:<6} {}{}",
            r.task_id, before, after, delta, verdict, note
        );
    }
    out
}

fn render_outcome(outcome: Option<GradeOutcome>) -> String {
    match outcome {
        Some(GradeOutcome::Scored(s)) => format!("{s:.2}"),
        Some(GradeOutcome::Skipped) => "skip".to_string(),
        None => "none".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn result(id: &str, before: f64, after: f64) -> RefuteProbeResult {
        RefuteProbeResult {
            task_id: id.to_string(),
            before: Some(GradeOutcome::Scored(before)),
            after: Some(GradeOutcome::Scored(after)),
            critique_ok: true,
            defense_ok: true,
        }
    }

    #[test]
    fn render_shows_before_after_delta_and_per_probe_verdict() {
        let results = vec![result("refute/a", 0.0, 1.0), result("refute/b", 0.5, 0.55)];
        let text = render_results(&results, 0.2);
        assert!(text.contains("refute/a"));
        assert!(text.contains("before=0.00"));
        assert!(text.contains("after=1.00"));
        assert!(text.contains("delta=+1.00"));
        assert!(text.contains("refute/b"));
        assert!(text.contains("delta=+0.05"));
    }

    #[test]
    fn render_explains_a_failed_leg_instead_of_a_bare_n_a() {
        let mut r = result("refute/c", 0.5, 0.5);
        r.after = None;
        r.defense_ok = false;
        let text = render_results(&[r], 0.2);
        assert!(text.contains("delta=n/a"));
        assert!(text.contains("defense leg failed"));
    }
}
