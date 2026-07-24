//! `agentpit outcome` — record the human's explicit good/bad verdict on a run.
//!
//! The highest-weight label source for the learned routing layer: the fold (Phase 1) trusts an
//! `OutcomeNoted` line over aggregator grades and exit statuses. Appends to the same best-effort
//! `events.jsonl` as every other event; `run_id` defaults to the most recent non-bench run.

use anyhow::{Context, Result};

use crate::events::{Event, OutcomeLabel, RunKind, RunLogger, events_path};

pub async fn run(verdict: String, run_id: Option<String>) -> Result<()> {
    let outcome = match verdict.to_ascii_lowercase().as_str() {
        "good" => OutcomeLabel::Good,
        "bad" => OutcomeLabel::Bad,
        other => anyhow::bail!("verdict must be `good` or `bad`, got `{other}`"),
    };

    let run_id = match run_id {
        Some(id) => id,
        None => {
            let log = std::fs::read_to_string(events_path())
                .with_context(|| format!("no event log at {}", events_path().display()))?;
            latest_run_id(&log).context(
                "no runs found in the event log; pass an explicit run id (`agentpit outcome good <run_id>`)",
            )?
        }
    };

    RunLogger::adopt(run_id.clone()).outcome(outcome);
    println!("[outcome={} run={run_id}]", outcome.as_str());
    Ok(())
}

/// The most recent dispatch run in the log: the last `RunStarted` line that isn't a gold-bench
/// sweep (a bench run is an evaluation, not a routed dispatch — "the last run I just did" never
/// means one of those).
fn latest_run_id(log: &str) -> Option<String> {
    log.lines()
        .filter_map(|line| serde_json::from_str::<Event>(line).ok())
        .filter_map(|event| match event {
            Event::RunStarted { run_id, kind, .. } if kind != RunKind::Bench => Some(run_id),
            _ => None,
        })
        .next_back()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latest_run_id_picks_last_non_bench_run_and_survives_junk_lines() {
        let log = r#"{"event":"run_started","ts":1,"run_id":"r-1","pid":1,"kind":"rescue","members":["claude"],"cwd":"/x"}
not json at all
{"event":"run_finished","ts":2,"run_id":"r-1","status":"ok"}
{"event":"run_started","ts":3,"run_id":"r-2","pid":1,"kind":"review","members":["codex"],"cwd":"/x"}
{"event":"run_started","ts":4,"run_id":"r-3","pid":1,"kind":"bench","members":["codex"],"cwd":"/x"}
"#;
        assert_eq!(latest_run_id(log).as_deref(), Some("r-2"));
        assert_eq!(latest_run_id(""), None);
    }
}
