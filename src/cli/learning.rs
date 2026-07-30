//! `agentpit learning [--json]` — what the routing layer has learned so far.
//!
//! `profile show` prints the matrix; this prints the *state of the learning*: how much of
//! the matrix is still a seeded guess, which cells are accruing evidence without reaching
//! the sample gate, how strong that evidence is, whether any routing decision has actually
//! moved off its prior, and how the learned policy scores against recorded telemetry.
//!
//! `--json` is the desktop dashboard's data source (`learning_status` Tauri command), so the
//! shape here is a contract with `dashboard/frontend/src/learning`.

use std::collections::{HashMap, HashSet};
use std::fs;

use anyhow::Result;
use console::style;
use serde::Serialize;

use crate::profile::status::{
    Cell, Coverage, DayBucket, Evidence, Pick, Row, SourceMix, coverage, evidence, label_mix,
    picks, rows, timeline,
};
use crate::profile::{ProfileSource, TaskCategory, load_profiles, profiles_path, seeded_profiles};
use crate::types::BackendId;

/// Days of history the timeline covers.
const TIMELINE_DAYS: usize = 14;

/// Where the numbers came from, so a surprising view can be traced to a file.
#[derive(Debug, Clone, Serialize)]
pub struct Sources {
    pub profiles: String,
    /// False when `profiles.toml` does not exist yet and the matrix is the built-in priors.
    pub profiles_persisted: bool,
    pub events: String,
    pub events_present: bool,
}

/// The kNN sample store's state. `built` is false in a binary compiled without
/// `--features similarity`: the samples may exist but this build cannot route on them.
#[derive(Debug, Clone, Serialize)]
pub struct Similarity {
    pub built: bool,
    pub enabled: bool,
    pub path: String,
    pub samples: usize,
    pub good: usize,
    pub bad: usize,
    pub min_samples: usize,
}

/// The learned policy's would-have-been accuracy over recorded telemetry.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Replay {
    pub decisions: usize,
    pub evaluable: usize,
    pub correct: usize,
}

/// How routing is configured right now, and where each category currently goes.
#[derive(Debug, Clone, Serialize)]
pub struct Routing {
    pub auto_route: bool,
    /// `[routes]` pins, as `tool=backend`. A pinned tool never consults learned capability.
    pub pinned: Vec<String>,
    pub quality_margin: u8,
    pub available: Vec<BackendId>,
    pub picks: Vec<Pick>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replay: Option<Replay>,
}

/// The whole report. Serialized verbatim by `--json`.
#[derive(Debug, Clone, Serialize)]
pub struct LearningStatus {
    pub generated_ts: u64,
    pub sources: Sources,
    pub min_samples: u16,
    pub runs: usize,
    pub labels: usize,
    pub coverage: Coverage,
    pub categories: Vec<TaskCategory>,
    pub rows: Vec<Row>,
    pub label_mix: SourceMix,
    pub timeline: Vec<DayBucket>,
    pub similarity: Similarity,
    pub routing: Routing,
}

pub fn run(json: bool) -> Result<()> {
    let status = collect()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&status)?);
    } else {
        print!("{}", render(&status));
    }
    Ok(())
}

/// Read every input the report needs and assemble it. Missing telemetry is an empty report,
/// not an error: a fresh install has nothing learned yet and that is a legitimate state to
/// display.
fn collect() -> Result<LearningStatus> {
    let min_samples = crate::profile::learn::DEFAULT_MIN_SAMPLES;

    let events_path = crate::events::events_path();
    let log = fs::read_to_string(&events_path).unwrap_or_default();
    let (runs, labels) = super::profile::labels_from_log(&log);

    let profiles = load_profiles(None)?;
    let seeded = seeded_profiles();
    let evidence = evidence(&labels, min_samples, &profiles);

    let ctx = super::load_context()?;
    let config = &ctx.loaded.config;
    let mut available: Vec<BackendId> = ctx.regs.available().into_iter().collect();
    available.sort();
    let available_set: HashSet<BackendId> = available.iter().copied().collect();
    let costs: HashMap<BackendId, u8> = config
        .backends
        .iter()
        .filter_map(|(backend, o)| o.cost.map(|c| (*backend, c)))
        .collect();

    // The replay needs labels to score against; without any it stays absent rather than
    // reporting a hollow 0%.
    let replay = if labels.is_empty() {
        None
    } else {
        super::profile::policy_report("learned", &labels)
            .ok()
            .map(|r| Replay {
                decisions: r.decisions,
                evaluable: r.evaluable,
                correct: r.correct,
            })
    };

    Ok(LearningStatus {
        generated_ts: crate::events::now_ms(),
        sources: Sources {
            profiles: profiles_path().display().to_string(),
            profiles_persisted: profiles_path().exists(),
            events: events_path.display().to_string(),
            events_present: !log.is_empty(),
        },
        min_samples,
        runs: runs.len(),
        labels: labels.len(),
        coverage: coverage(&profiles),
        categories: TaskCategory::ALL.to_vec(),
        rows: rows(&profiles, &evidence),
        label_mix: label_mix(&labels),
        timeline: timeline(&labels, crate::events::now_ms(), TIMELINE_DAYS),
        similarity: similarity_status(config),
        routing: Routing {
            auto_route: config.default.auto_route,
            pinned: config
                .routes
                .iter()
                .map(|(tool, backend)| format!("{tool}={backend}"))
                .collect(),
            quality_margin: config.auto_route.quality_margin,
            available,
            picks: picks(
                &profiles,
                &seeded,
                &available_set,
                config.auto_route.quality_margin,
                &costs,
            ),
            replay,
        },
    })
}

fn similarity_status(config: &crate::config::HubConfig) -> Similarity {
    let path = crate::similarity::routes_path();
    let samples = fs::read_to_string(&path)
        .map(|raw| crate::similarity::parse_samples(&raw))
        .unwrap_or_default();
    let good = samples.iter().filter(|s| s.is_good()).count();
    Similarity {
        built: cfg!(feature = "similarity"),
        enabled: config.auto_route.similarity.enabled,
        path: path.display().to_string(),
        samples: samples.len(),
        good,
        bad: samples.len() - good,
        min_samples: config.auto_route.similarity.min_samples,
    }
}

/// Terminal rendering. Pure: builds and returns a fresh `String`.
fn render(status: &LearningStatus) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    let _ = writeln!(
        out,
        "learning status  ({} runs / {} labels · {})",
        status.runs, status.labels, status.sources.events
    );
    if !status.sources.events_present {
        let _ = writeln!(
            out,
            "  no telemetry yet — dispatch some work, then `agentpit profile learn`"
        );
    }

    let c = status.coverage;
    let _ = writeln!(
        out,
        "\ncoverage  {} cells: {} benchmarked · {} learned · {} still seeded",
        c.total,
        style(c.benchmarked).cyan(),
        style(c.learned).cyan(),
        style(c.seeded).yellow(),
    );

    let mix = status.label_mix;
    let _ = writeln!(
        out,
        "evidence  outcome {} · grade {} · rerun {} · exit {}  (gate: {} labels per cell)",
        mix.outcome, mix.grade, mix.rerun, mix.exit, status.min_samples
    );

    // Cells with telemetry behind them, strongest evidence first — the actual "in progress".
    let mut pending: Vec<(BackendId, &Cell, &Evidence)> = status
        .rows
        .iter()
        .flat_map(|row| {
            row.cells
                .iter()
                .filter_map(move |cell| cell.evidence.as_ref().map(|e| (row.backend, cell, e)))
        })
        .collect();
    pending.sort_by_key(|(_, _, evidence)| std::cmp::Reverse(evidence.labels));
    if pending.is_empty() {
        let _ = writeln!(out, "\nno cell has any labelled evidence yet.");
    } else {
        let _ = writeln!(out, "\nevidence per cell");
        for (backend, cell, e) in pending {
            let note = if e.outranked {
                " (benchmarked cell — learned cannot overwrite)"
            } else if e.promoted {
                " (promoted)"
            } else {
                ""
            };
            let _ = writeln!(
                out,
                "  {:<12} {:<18} {:>2}/{:<2} labels  good {} / bad {}  → would score {}{}",
                backend.as_str(),
                cell.category.as_str(),
                e.labels,
                status.min_samples,
                e.good,
                e.bad,
                e.projected,
                note,
            );
        }
    }

    let _ = writeln!(out, "\nrouting  auto_route={}", status.routing.auto_route);
    if !status.routing.pinned.is_empty() {
        let _ = writeln!(
            out,
            "  note: [routes] pins {} — capability routing does not run for them",
            status.routing.pinned.join(", ")
        );
    }
    for pick in &status.routing.picks {
        let Some(backend) = pick.backend else {
            continue;
        };
        let source = pick
            .source
            .map(|s| s.as_str())
            .unwrap_or(ProfileSource::Seeded.as_str());
        let changed = match pick.seeded_backend {
            Some(seeded) if pick.changed => format!("  (was {seeded} under the seeded priors)"),
            _ => String::new(),
        };
        let _ = writeln!(
            out,
            "  {:<18} → {:<12} {} {}{}",
            pick.category.as_str(),
            backend.as_str(),
            pick.score.unwrap_or(0),
            source,
            changed,
        );
    }

    if let Some(replay) = status.routing.replay {
        let _ = writeln!(
            out,
            "\nreplay (learned policy)  {} decisions, {} evaluable, {} would-have-gone-well ({}%)",
            replay.decisions,
            replay.evaluable,
            replay.correct,
            (replay.correct * 100)
                .checked_div(replay.evaluable)
                .unwrap_or(0),
        );
    }

    let s = &status.similarity;
    let _ = writeln!(
        out,
        "similarity  {} · {} sample(s) (good {} / bad {}), needs {}",
        if !s.built {
            "not in this build"
        } else if s.enabled {
            "on"
        } else {
            "disabled in config"
        },
        s.samples,
        s.good,
        s.bad,
        s.min_samples,
    );

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status() -> LearningStatus {
        LearningStatus {
            generated_ts: 1_000,
            sources: Sources {
                profiles: "/x/profiles.toml".into(),
                profiles_persisted: true,
                events: "/x/events.jsonl".into(),
                events_present: true,
            },
            min_samples: 5,
            runs: 12,
            labels: 7,
            coverage: Coverage {
                total: 20,
                seeded: 17,
                learned: 3,
                benchmarked: 0,
            },
            categories: TaskCategory::ALL.to_vec(),
            rows: vec![Row {
                backend: BackendId::Codex,
                summary_source: ProfileSource::Learned,
                measured_at: None,
                cells: vec![Cell {
                    category: TaskCategory::Coding,
                    value: 67,
                    confidence: 0.4,
                    samples: 3,
                    source: ProfileSource::Learned,
                    evidence: Some(Evidence {
                        labels: 3,
                        good: 2,
                        bad: 1,
                        mix: SourceMix {
                            outcome: 1,
                            grade: 1,
                            rerun: 0,
                            exit: 1,
                        },
                        projected: 80,
                        projected_confidence: 0.45,
                        promoted: false,
                        outranked: false,
                        last_ts: 900,
                    }),
                }],
            }],
            label_mix: SourceMix {
                outcome: 2,
                grade: 3,
                rerun: 1,
                exit: 1,
            },
            timeline: vec![DayBucket {
                start_ms: 0,
                labels: 7,
                good: 5,
                bad: 2,
            }],
            similarity: Similarity {
                built: false,
                enabled: true,
                path: "/x/routes.jsonl".into(),
                samples: 0,
                good: 0,
                bad: 0,
                min_samples: 3,
            },
            routing: Routing {
                auto_route: true,
                pinned: vec!["review=codex".into()],
                quality_margin: 5,
                available: vec![BackendId::Claude, BackendId::Codex],
                picks: vec![Pick {
                    category: TaskCategory::Coding,
                    backend: Some(BackendId::Codex),
                    score: Some(67),
                    source: Some(ProfileSource::Learned),
                    cost_tiebreak: false,
                    seeded_backend: Some(BackendId::Claude),
                    changed: true,
                }],
                replay: Some(Replay {
                    decisions: 9,
                    evaluable: 3,
                    correct: 2,
                }),
            },
        }
    }

    #[test]
    fn render_reports_the_gate_the_pin_and_the_moved_decision() {
        let text = render(&status());
        assert!(text.contains("12 runs / 7 labels"));
        assert!(text.contains("still seeded"));
        assert!(text.contains("3/5"), "the sample gate is visible: {text}");
        assert!(text.contains("would score 80"));
        assert!(text.contains("[routes] pins review=codex"));
        assert!(
            text.contains("was claude under the seeded priors"),
            "a moved decision names the prior: {text}"
        );
        assert!(text.contains("66%"), "replay accuracy is rendered: {text}");
        assert!(text.contains("not in this build"));
    }

    /// The dashboard reads these keys. A rename here is a breaking change for it, so the
    /// contract is pinned by a test rather than left to the frontend to discover at runtime.
    #[test]
    fn json_carries_the_keys_the_dashboard_reads() {
        let json = serde_json::to_value(status()).unwrap();
        for key in [
            "coverage",
            "rows",
            "label_mix",
            "timeline",
            "similarity",
            "routing",
            "min_samples",
        ] {
            assert!(json.get(key).is_some(), "missing top-level key {key}");
        }
        let cell = &json["rows"][0]["cells"][0];
        assert_eq!(cell["source"], "learned");
        assert_eq!(cell["evidence"]["labels"], 3);
        assert_eq!(cell["evidence"]["projected"], 80);
        assert_eq!(json["routing"]["picks"][0]["category"], "coding");
        assert_eq!(json["routing"]["replay"]["evaluable"], 3);
    }
}
