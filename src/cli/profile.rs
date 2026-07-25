//! `agentpit profile show | seed [--force] | reset` — inspect and (re)seed the
//! machine-generated capability matrix in `profiles.toml`.
//!
//! - **show** renders the backend×category matrix (score / confidence / source).
//! - **seed** writes the hand-seeded priors, refusing to clobber an existing file unless
//!   `--force` is passed.
//! - **reset** clears every measured value, restoring the seeded priors.
//!
//! All three operations are additive to the public CLI surface and never touch the
//! hand-written `config.toml`.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::Subcommand;
use console::style;
use tokio_util::sync::CancellationToken;

use crate::events::{LegStatus, RunKind, RunLogger, output_streamer};
use crate::profile::bench::{
    RawFixture, RawScored, ReplayFixture, all_tasks, merge_into_profiles, run_live, score_fixture,
    score_raw,
};
use crate::profile::{
    BenchmarkResult, CapabilityProfile, ProfileSet, apply_benchmark, load_profiles, profiles_path,
    save_profiles, seeded_profiles,
};
use crate::types::BackendId;

#[derive(Subcommand, Debug)]
pub enum Action {
    /// Print the capability matrix (backend × category: score / confidence / source).
    Show,
    /// Write the hand-seeded priors to profiles.toml (refuses to clobber unless --force).
    Seed {
        /// Overwrite an existing profiles.toml.
        #[arg(long)]
        force: bool,
    },
    /// Clear all measured values, restoring the seeded priors.
    Reset,
    /// Measure a backend with the gold-bench suite and merge the scores into profiles.toml.
    ///
    /// Modes: `--backend <id>` runs the suite live (dispatches each gold task, grades it in a
    /// network-isolated sandbox, merges the scores); `--raw-replay <file>` re-grades recorded
    /// raw outputs offline (no network); `--replay <file>` folds already-graded `passed/total`
    /// counts; bare `--dry-run` prints the suite plan without running anything.
    Run {
        /// Target backend for a live run. With `--replay`/`--raw-replay` it must match the
        /// fixture's backend; otherwise the fixture's own backend is used.
        #[arg(long)]
        backend: Option<BackendId>,
        /// Replay a recorded fixture (JSON) of already-graded per-task `passed/total` counts.
        #[arg(long, value_name = "FILE")]
        replay: Option<PathBuf>,
        /// Re-grade a recorded fixture (JSON) of raw per-task outputs through the real graders.
        #[arg(long, value_name = "FILE")]
        raw_replay: Option<PathBuf>,
        /// During a live run, also write the captured raw outputs here for later re-grading.
        #[arg(long, value_name = "FILE")]
        save_raw: Option<PathBuf>,
        /// Score and report, but do not write profiles.toml.
        #[arg(long)]
        dry_run: bool,
    },
    /// Replay past telemetry against a routing policy and report its would-have-been
    /// accuracy: for each recorded routing decision (one per distinct task+category),
    /// where would the policy have routed, and does the folded verdict for that
    /// (task, category, backend) say it went well?
    Replay {
        /// `seeded` (the profile stage over the hand-seeded priors), `learned` (the
        /// similarity stage when available, then the profile stage with cost tiebreak
        /// over the current profiles.toml), or `similarity` (kNN stage alone; needs a
        /// `--features similarity` build).
        #[arg(long, default_value = "learned")]
        policy: String,
    },
    /// Fold runtime telemetry (events.jsonl) into Learned scores and merge them into
    /// profiles.toml. Benchmarked cells are never overwritten; cells with fewer than
    /// --min-samples labels are skipped.
    Learn {
        /// Print the would-be changes without writing profiles.toml.
        #[arg(long)]
        dry_run: bool,
        /// Minimum labels a (backend, category) cell needs before it is written.
        #[arg(long, default_value_t = crate::profile::learn::DEFAULT_MIN_SAMPLES)]
        min_samples: u16,
    },
}

/// Entry point. A bare `agentpit profile` (no sub-action) defaults to `show`.
pub async fn run(action: Option<Action>) -> Result<()> {
    match action.unwrap_or(Action::Show) {
        Action::Show => show(),
        Action::Seed { force } => seed(&profiles_path(), force),
        Action::Reset => reset(&profiles_path()),
        Action::Run {
            backend,
            replay,
            raw_replay,
            save_raw,
            dry_run,
        } => {
            run_bench(
                backend,
                replay.as_deref(),
                raw_replay.as_deref(),
                save_raw.as_deref(),
                dry_run,
                &profiles_path(),
            )
            .await
        }
        Action::Learn {
            dry_run,
            min_samples,
        } => learn(dry_run, min_samples, &profiles_path()),
        Action::Replay { policy } => replay(&policy),
    }
}

/// A replay policy's routing choice for one labelled run.
type PolicyPicker = Box<dyn Fn(&crate::profile::learn::Label) -> Option<BackendId>>;

/// One folded verdict's identity: the task, the category it was routed as, and the backend
/// that ran it. Scoring is per `(task, category)`, so the category has to be part of this.
type VerdictKey = (String, crate::profile::TaskCategory, BackendId);

/// What `replay_core` measured: routing decisions replayed, how many were evaluable, and
/// how many of those the policy would have gotten right.
struct ReplayReport {
    labels: usize,
    decisions: usize,
    evaluable: usize,
    correct: usize,
}

/// Collapse all labels for one `(task, category, backend)` into a single verdict by
/// weighted majority (label weights already rank the sources: human verdict > grade >
/// rerun > exit). A tie goes to the most recent label. This replaces the old OR fold,
/// where one stray exit-ok label outvoted any number of bad verdicts.
///
/// The category belongs in the key: scoring happens per `(task, category)` decision, so a
/// verdict folded across categories would let a backend's outcome on one category decide a
/// different category's decision for the same task.
fn fold_verdicts(
    labels: &[crate::profile::learn::Label],
) -> std::collections::HashMap<VerdictKey, bool> {
    struct Acc {
        good: f32,
        bad: f32,
        latest_ts: u64,
        latest_success: bool,
    }
    let mut acc: std::collections::HashMap<VerdictKey, Acc> = Default::default();
    for label in labels {
        let Some(hash) = &label.task_hash else {
            continue;
        };
        let entry = acc
            .entry((hash.clone(), label.category, label.backend))
            .or_insert(Acc {
                good: 0.0,
                bad: 0.0,
                latest_ts: 0,
                latest_success: label.success,
            });
        if label.success {
            entry.good += label.weight;
        } else {
            entry.bad += label.weight;
        }
        if label.ts >= entry.latest_ts {
            entry.latest_ts = label.ts;
            entry.latest_success = label.success;
        }
    }
    acc.into_iter()
        .map(|(key, a)| {
            let verdict = if a.good != a.bad {
                a.good > a.bad
            } else {
                a.latest_success
            };
            (key, verdict)
        })
        .collect()
}

/// Score a policy against the recorded labels. Evaluation is per routing decision — one
/// per distinct `(task, category)` — not per label, so a task that was rerun five times
/// (or graded for three ensemble members) no longer counts five times for the identical,
/// deterministic pick.
fn replay_core(labels: &[crate::profile::learn::Label], pick_for: &PolicyPicker) -> ReplayReport {
    let verdicts = fold_verdicts(labels);
    let mut seen: std::collections::HashSet<(String, crate::profile::TaskCategory)> =
        Default::default();
    let mut report = ReplayReport {
        labels: labels.len(),
        decisions: 0,
        evaluable: 0,
        correct: 0,
    };
    for label in labels {
        let Some(hash) = &label.task_hash else {
            continue;
        };
        if !seen.insert((hash.clone(), label.category)) {
            continue;
        }
        report.decisions += 1;
        let Some(pick) = pick_for(label) else {
            continue;
        };
        let Some(went_well) = verdicts.get(&(hash.clone(), label.category, pick)) else {
            continue; // nobody ran this backend on this task *in this category*
        };
        report.evaluable += 1;
        if *went_well {
            report.correct += 1;
        }
    }
    report
}

/// The deployed router's profile stage, reproduced for replay: full candidate set for the
/// category, then [`pick_with_cost_tiebreak`](crate::router::pick_with_cost_tiebreak) with
/// the live config's `quality_margin` and per-backend costs — not a raw `best_for` argmax.
fn profile_stage_picker(
    set: crate::profile::ProfileSet,
    available: std::collections::HashSet<BackendId>,
    margin: u8,
    costs: std::collections::HashMap<BackendId, u8>,
) -> PolicyPicker {
    Box::new(move |l: &crate::profile::learn::Label| {
        let candidates = set.candidates_for(l.category, &available);
        crate::router::pick_with_cost_tiebreak(&candidates, margin, |b| {
            costs.get(&b).copied().unwrap_or(50)
        })
        .map(|(b, _, _)| b)
    })
}

/// `agentpit profile replay`: score a routing policy against the recorded labels.
///
/// Scope: this replays the *policy* stages of [`Router::resolve`](crate::router::Router)
/// — the similarity stage and the capability-profile stage with its cost tiebreak, using
/// the live config's `quality_margin` and per-backend costs. It deliberately does NOT
/// replay the stages that bypass the policy: a `[routes]` pin, the capacity gate (a task
/// over `long_context_threshold` goes to `long_context_backend` before any policy runs),
/// the diagnose confidence gate, the keyword fallback, or the default backend. So the number
/// answers "how good are this policy's picks?", not "what would dispatch have done?" —
/// on a machine whose `[routes]` pins a tool, dispatch would not have consulted the
/// policy at all.
///
/// Honest scoring caveat: we only know how backends that actually ran performed, so a pick
/// is *evaluable* only when some label exists for the same task on the picked backend.
/// The report separates coverage (evaluable/decisions) from accuracy (correct/evaluable).
fn replay(policy: &str) -> Result<()> {
    use crate::profile::learn::{
        DEFAULT_RERUN_WINDOW_MS, derive_labels, parse_runs, resolve_categories,
    };

    let events = crate::events::events_path();
    let log = fs::read_to_string(&events)
        .with_context(|| format!("no telemetry at {}", events.display()))?;
    let mut runs = parse_runs(&log);
    resolve_categories(&mut runs, |task_hash| {
        let text =
            fs::read_to_string(crate::events::tasks_dir().join(format!("{task_hash}.txt"))).ok()?;
        Some(crate::diagnose::diagnose(&text).primary)
    });
    let labels = derive_labels(&runs, DEFAULT_RERUN_WINDOW_MS);
    if labels.is_empty() {
        bail!("no labelled runs to replay yet");
    }

    // Every backend ever labelled counts as available for the what-if.
    let available: std::collections::HashSet<BackendId> =
        labels.iter().map(|l| l.backend).collect();

    // The deployed router's tiebreak inputs come from the live config, so `learned`
    // replays what this machine's router would actually do.
    let config = super::load_context()?.loaded.config;
    let margin = config.auto_route.quality_margin;
    let costs: std::collections::HashMap<BackendId, u8> = config
        .backends
        .iter()
        .filter_map(|(backend, o)| o.cost.map(|c| (*backend, c)))
        .collect();

    let pick_for: PolicyPicker = match policy {
        "seeded" => {
            profile_stage_picker(seeded_profiles(), available.clone(), margin, costs.clone())
        }
        "learned" => {
            // The deployed auto-route chain: similarity stage first (when this build has
            // it and samples exist — leave-one-out, so a task never matches itself), then
            // the profile stage over the current profiles.toml.
            let profile = profile_stage_picker(
                load_profiles(None)?,
                available.clone(),
                margin,
                costs.clone(),
            );
            match similarity_replay_picker(&available)? {
                Some(sim) => {
                    Box::new(move |l: &crate::profile::learn::Label| sim(l).or_else(|| profile(l)))
                }
                None => profile,
            }
        }
        "similarity" => similarity_replay_picker(&available)?.ok_or_else(|| {
            anyhow!(
                "similarity policy unavailable: needs a --features similarity build and \
                 recorded samples (run `agentpit similarity init`, then `agentpit profile learn`)"
            )
        })?,
        other => bail!("unknown policy `{other}` (expected seeded|learned|similarity)"),
    };

    let report = replay_core(&labels, &pick_for);
    println!(
        "policy={policy}: {} labels -> {} routing decisions, {} evaluable, {} would-have-gone-well ({}%)",
        report.labels,
        report.decisions,
        report.evaluable,
        report.correct,
        (report.correct * 100)
            .checked_div(report.evaluable)
            .unwrap_or(0),
    );
    if report.evaluable < report.decisions {
        println!(
            "(coverage note: {} decision(s) had no label for the policy's pick and were skipped)",
            report.decisions - report.evaluable
        );
    }
    Ok(())
}

/// The similarity stage's picker: leave-one-out kNN over the stored samples. `Ok(None)`
/// when this build lacks the feature or no samples are recorded yet — the `learned`
/// policy falls through to the profile stage in that case, mirroring the deployed router.
#[cfg(feature = "similarity")]
fn similarity_replay_picker(
    available: &std::collections::HashSet<BackendId>,
) -> Result<Option<PolicyPicker>> {
    use crate::similarity::{parse_samples, pick_backend, routes_path};

    let Ok(raw) = fs::read_to_string(routes_path()) else {
        return Ok(None);
    };
    let samples = parse_samples(&raw);
    let cfg = super::load_context()?.loaded.config.auto_route.similarity;
    let available = available.clone();
    Ok(Some(Box::new(
        move |label: &crate::profile::learn::Label| {
            let hash = label.task_hash.as_deref()?;
            let query = samples
                .iter()
                .find(|s| s.task_hash == hash)?
                .embedding
                .clone();
            let others: Vec<_> = samples
                .iter()
                .filter(|s| s.task_hash != hash)
                .cloned()
                .collect();
            pick_backend(&query, &others, &cfg, |b| available.contains(&b)).map(|p| p.backend)
        },
    )))
}

#[cfg(not(feature = "similarity"))]
fn similarity_replay_picker(
    _available: &std::collections::HashSet<BackendId>,
) -> Result<Option<PolicyPicker>> {
    Ok(None)
}

/// `agentpit profile learn`: fold the event log into Learned scores and merge them into
/// `profiles.toml` under the `benchmarked > learned > seeded` gate.
fn learn(dry_run: bool, min_samples: u16, profiles: &Path) -> Result<()> {
    use crate::profile::learn::{
        DEFAULT_RERUN_WINDOW_MS, derive_labels, fold_scores, parse_runs, resolve_categories,
    };

    let events = crate::events::events_path();
    let log = fs::read_to_string(&events).with_context(|| {
        format!(
            "no telemetry at {} — dispatch some runs first (or check AGENTPIT_NO_EVENTS)",
            events.display()
        )
    })?;

    let mut runs = parse_runs(&log);
    // Runs routed without a category (route table / default / ensemble) get one by
    // re-diagnosing the task text saved at dispatch time.
    resolve_categories(&mut runs, |task_hash| {
        let text =
            fs::read_to_string(crate::events::tasks_dir().join(format!("{task_hash}.txt"))).ok()?;
        Some(crate::diagnose::diagnose(&text).primary)
    });
    let labels = derive_labels(&runs, DEFAULT_RERUN_WINDOW_MS);
    let scores = fold_scores(&labels, min_samples);
    println!(
        "telemetry: {} runs, {} labels -> {} cell(s) with >= {} samples",
        runs.len(),
        labels.len(),
        scores.values().map(|c| c.len()).sum::<usize>(),
        min_samples,
    );
    // The similarity store accrues from the labels directly — its evidence thresholds are
    // its own ([auto_route.similarity].min_samples), independent of whether any profile
    // cell reached --min-samples this time.
    if !dry_run {
        #[cfg(feature = "similarity")]
        update_route_samples(&labels);
    }
    if scores.is_empty() {
        println!("nothing to write yet.");
        return Ok(());
    }

    let base = load_profiles(Some(profiles))?;
    let mut merged = base.clone();
    let mut changed = 0usize;
    for (backend, learned) in &scores {
        let before = base
            .get(*backend)
            .cloned()
            .unwrap_or_else(|| CapabilityProfile::seeded(*backend));
        let after = crate::profile::apply_learned(&before, learned);
        for (category, score) in learned {
            let old = before.score(*category);
            let new = after.score(*category);
            if old == new {
                continue;
            }
            changed += 1;
            println!(
                "  [{backend}] {:<18} {} -> {}  (samples={}, conf={:.2})",
                category.as_str(),
                old.map(|s| s.value.to_string())
                    .unwrap_or_else(|| "-".into()),
                score.value,
                score.samples,
                score.confidence,
            );
        }
        let frozen = learned
            .keys()
            .filter(|category| {
                before.score(**category).map(|s| s.source)
                    == Some(crate::profile::ProfileSource::Benchmarked)
            })
            .count();
        if frozen > 0 {
            println!("  [{backend}] {frozen} benchmarked cell(s) left untouched");
        }
        merged.insert(after);
    }

    if changed == 0 {
        println!("no cells changed.");
        return Ok(());
    }
    if dry_run {
        println!("(dry run: {} not written)", profiles.display());
        return Ok(());
    }
    save_profiles(&merged, profiles)?;
    println!("wrote {} ({changed} cell(s) updated).", profiles.display());
    Ok(())
}

/// Sync the similarity layer's sample store with this fold's labels.
///
/// - The strongest label per `(task, backend)` wins (existing verdicts are *updated*, not
///   frozen — a later human verdict overrides a stored exit-status sample; the embedding
///   is reused so only genuinely new tasks are embedded).
/// - Expired evidence stays expired: samples past the TTL are dropped, and labels whose
///   own timestamp is past the TTL are never (re-)ingested.
/// - Best-effort — a missing model just prints a hint (`agentpit similarity init`).
#[cfg(feature = "similarity")]
fn update_route_samples(labels: &[crate::profile::learn::Label]) {
    use crate::similarity::{
        RouteSample, SAMPLE_TTL_MS, embed, parse_samples, routes_path, serialize_samples,
    };
    type Key = (String, BackendId);

    if !embed::model_ready() {
        println!(
            "similarity: model not installed; skipped sample ingestion (run `agentpit similarity init`)."
        );
        return;
    }

    let now = crate::events::now_ms();
    let existing = std::fs::read_to_string(routes_path())
        .map(|raw| parse_samples(&raw))
        .unwrap_or_default();
    let mut store: std::collections::BTreeMap<Key, RouteSample> = existing
        .into_iter()
        .filter(|s| now.saturating_sub(s.ts) <= SAMPLE_TTL_MS)
        .map(|s| ((s.task_hash.clone(), s.backend), s))
        .collect();

    // Strongest label per (task, backend) — weight first (human > grade > exit), then
    // recency. This is what may overwrite a stored verdict.
    let mut best: std::collections::BTreeMap<Key, &crate::profile::learn::Label> =
        Default::default();
    for label in labels {
        let Some(hash) = label.task_hash.clone() else {
            continue;
        };
        if label.ts > 0 && now.saturating_sub(label.ts) > SAMPLE_TTL_MS {
            continue; // an expired sample must not resurrect from its old event lines
        }
        best.entry((hash, label.backend))
            .and_modify(|held| {
                if (label.weight, label.ts) > (held.weight, held.ts) {
                    *held = label;
                }
            })
            .or_insert(label);
    }

    let mut pending: Vec<(Key, &crate::profile::learn::Label)> = Vec::new();
    let mut texts: Vec<String> = Vec::new();
    for (key, label) in best {
        let verdict = if label.success { "good" } else { "bad" };
        if let Some(sample) = store.get_mut(&key) {
            // Update in place only when the new evidence is at least as strong.
            if label.weight >= sample.weight {
                sample.label = verdict.into();
                sample.weight = label.weight;
                sample.category = Some(label.category.as_str().to_string());
                sample.ts = if label.ts > 0 { label.ts } else { sample.ts };
            }
            continue;
        }
        let Ok(text) =
            std::fs::read_to_string(crate::events::tasks_dir().join(format!("{}.txt", key.0)))
        else {
            continue;
        };
        pending.push((key, label));
        texts.push(text);
    }

    if !pending.is_empty() {
        let embeddings = match embed::embed_texts(&texts) {
            Ok(embeddings) => embeddings,
            Err(error) => {
                eprintln!("similarity: embedding failed, new samples not added: {error:#}");
                Vec::new()
            }
        };
        for (((hash, backend), label), embedding) in pending.into_iter().zip(embeddings) {
            store.insert(
                (hash.clone(), backend),
                RouteSample {
                    task_hash: hash,
                    embedding,
                    backend,
                    label: if label.success { "good" } else { "bad" }.into(),
                    weight: label.weight,
                    category: Some(label.category.as_str().to_string()),
                    ts: if label.ts > 0 { label.ts } else { now },
                },
            );
        }
    }

    let samples: Vec<RouteSample> = store.into_values().collect();
    let path = routes_path();
    let tmp = path.with_extension("jsonl.tmp");
    if std::fs::write(&tmp, serialize_samples(&samples)).is_ok()
        && std::fs::rename(&tmp, &path).is_ok()
    {
        println!(
            "similarity: {} sample(s) in {}.",
            samples.len(),
            path.display()
        );
    }
}

fn show() -> Result<()> {
    let path = profiles_path();
    let set = load_profiles(None)?;
    let persisted = path.exists();
    print!("{}", render_show(&set, &path, persisted));
    Ok(())
}

/// Render the capability matrix as a per-backend section list. Pure: builds and returns a
/// fresh `String`, mutating nothing.
fn render_show(set: &ProfileSet, path: &Path, persisted: bool) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    if persisted {
        let _ = writeln!(out, "profiles: {}", path.display());
    } else {
        let _ = writeln!(
            out,
            "profiles: {} (seeded defaults — not yet written; run `agentpit profile seed`)",
            path.display()
        );
    }

    if set.is_empty() {
        let _ = writeln!(out, "\n(no profiles)");
        return out;
    }

    for (backend, profile) in set.iter() {
        out.push_str(&render_backend_section(*backend, profile));
    }

    out
}

/// Render one backend's section of the capability matrix (header line plus its score rows).
/// Pure: builds and returns a fresh `String`, mutating nothing.
fn render_backend_section(backend: BackendId, profile: &CapabilityProfile) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    let measured = profile
        .measured_at
        .as_deref()
        .map(|m| format!("  measured_at={m}"))
        .unwrap_or_default();
    let _ = writeln!(
        out,
        "\n[{}]  source={}{}",
        style(backend).cyan(),
        profile.source,
        measured
    );

    if profile.scores.is_empty() {
        let _ = writeln!(out, "  (no scores)");
        return out;
    }

    // `src` is each cell's own provenance: s = seeded prior, L = telemetry-learned,
    // B = benchmarked. The header's `source=` is just the highest-priority cell present.
    let _ = writeln!(
        out,
        "  {:<18} {:>5}  {:>5}  {:>7}  {:>3}",
        "category", "score", "conf", "samples", "src"
    );
    for (category, score) in &profile.scores {
        let cell_source = match score.source {
            crate::profile::ProfileSource::Seeded => "s",
            crate::profile::ProfileSource::Learned => "L",
            crate::profile::ProfileSource::Benchmarked => "B",
        };
        let _ = writeln!(
            out,
            "  {:<18} {:>5}  {:>5.2}  {:>7}  {:>3}",
            category.as_str(),
            score.value,
            score.confidence,
            score.samples,
            cell_source
        );
    }

    out
}

/// Write the seeded priors to `path`. Refuses to overwrite an existing file unless `force`.
fn seed(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!(
            "{} already exists; pass --force to overwrite",
            path.display()
        );
    }
    let set = seeded_profiles();
    save_profiles(&set, path)?;
    println!(
        "seeded {} profiles → {}",
        set.len(),
        style(path.display()).green()
    );
    Ok(())
}

/// Clear measured values by rewriting the seeded priors. The seed set carries zero samples
/// and `source = seeded`, so this wipes any benchmarked/learned readings back to the baseline.
fn reset(path: &Path) -> Result<()> {
    let set = seeded_profiles();
    save_profiles(&set, path)?;
    println!(
        "reset capability profiles to seeded priors → {}",
        style(path.display()).green()
    );
    Ok(())
}

/// `profile run` dispatch. `--backend` runs the suite live; `--raw-replay` re-grades recorded raw
/// outputs offline; `--replay` folds pre-graded counts; bare `--dry-run` prints the suite plan.
async fn run_bench(
    backend: Option<BackendId>,
    replay: Option<&Path>,
    raw_replay: Option<&Path>,
    save_raw: Option<&Path>,
    dry_run: bool,
    profiles: &Path,
) -> Result<()> {
    match (replay, raw_replay) {
        (Some(_), Some(_)) => bail!("--replay and --raw-replay are mutually exclusive"),
        (Some(fixture), None) => replay_fixture(fixture, backend, dry_run, profiles),
        (None, Some(fixture)) => raw_replay_fixture(fixture, backend, dry_run, profiles),
        (None, None) => match backend {
            Some(b) => run_live_bench(b, save_raw, dry_run, profiles).await,
            None if dry_run => {
                print!("{}", render_plan());
                Ok(())
            }
            None => bail!(
                "specify a target: `--backend <id>` for a live run, `--raw-replay <file>` / \
                 `--replay <file>` to score a recording, or `--dry-run` to print the suite plan"
            ),
        },
    }
}

/// Run the gold-bench suite live against `backend`: check availability + auth, dispatch every
/// task, grade the captured outputs through the real scorers, and merge. `--save-raw` persists the
/// raw outputs for later offline re-grading.
async fn run_live_bench(
    backend: BackendId,
    save_raw: Option<&Path>,
    dry_run: bool,
    profiles: &Path,
) -> Result<()> {
    let ctx = super::load_context()?;
    if !ctx.regs.available().contains(&backend) {
        bail!("backend {backend} is not available in this environment");
    }
    let auth = crate::auth::check_auth(backend).await;
    if !auth.ok {
        bail!(
            "[{backend}] not authenticated. Run `{}`, or call `agentpit login {backend}`.",
            auth.login_command
        );
    }

    let cwd = super::resolve_cwd(None)?;
    let cancel = CancellationToken::new();
    super::install_ctrlc_cancel(cancel.clone());

    let tasks = all_tasks();
    eprintln!(
        "running {} gold task(s) against [{}] …",
        tasks.len(),
        backend
    );

    // Make the sweep visible in the dashboard swarm: a single-member `bench` run whose member
    // streams every task's output to the run's capture file, so it can be tailed live instead of
    // running invisibly. The bench is sequential single-backend, so one member is the honest shape.
    let logger = RunLogger::start(RunKind::Bench, &[backend], &cwd);
    logger.member_started(backend, false);
    let started = std::time::Instant::now();
    let sink = output_streamer(logger.run_id(), backend, false);

    let fixture = match run_live(backend, &tasks, &cwd, &ctx.regs, None, cancel, sink).await {
        Ok(f) => f,
        Err(e) => {
            logger.member_finished(
                backend,
                false,
                LegStatus::Error,
                started.elapsed().as_millis() as u64,
                None,
                Some(format!("{e:#}")),
            );
            logger.finished(LegStatus::Error);
            return Err(e);
        }
    };
    let total_chars: usize = fixture.outputs.iter().map(|o| o.output.len()).sum();
    logger.member_finished(
        backend,
        false,
        LegStatus::Ok,
        started.elapsed().as_millis() as u64,
        Some(total_chars),
        None,
    );
    logger.finished(LegStatus::Ok);

    if let Some(out) = save_raw {
        let json = serde_json::to_string_pretty(&fixture)?;
        fs::write(out, json)
            .with_context(|| format!("failed to write raw fixture {}", out.display()))?;
        eprintln!("saved raw outputs → {}", out.display());
    }

    let scored = score_raw(&tasks, &fixture)?;
    finish_bench(backend, scored, dry_run, profiles)
}

/// Re-grade a recorded raw-output fixture offline (no network), then merge. The graders run for
/// real — this is the offline path that exercises `judge`.
fn raw_replay_fixture(
    path: &Path,
    backend: Option<BackendId>,
    dry_run: bool,
    profiles: &Path,
) -> Result<()> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read raw fixture {}", path.display()))?;
    let fixture: RawFixture = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse raw fixture {}", path.display()))?;

    if let Some(requested) = backend
        && requested != fixture.backend
    {
        bail!(
            "--backend {requested} does not match fixture backend {}",
            fixture.backend
        );
    }

    let tasks = all_tasks();
    let scored = score_raw(&tasks, &fixture)?;
    finish_bench(fixture.backend, scored, dry_run, profiles)
}

/// Report skipped tasks, merge the scored result into `profiles.toml` (unless `dry_run`), and
/// print the per-category outcome. Shared by the live and raw-replay paths.
fn finish_bench(
    backend: BackendId,
    scored: RawScored,
    dry_run: bool,
    profiles: &Path,
) -> Result<()> {
    if !scored.skipped.is_empty() {
        eprintln!(
            "skipped {} task(s) (sandbox unavailable — not scored): {}",
            scored.skipped.len(),
            scored.skipped.join(", ")
        );
    }

    let merged = if dry_run {
        let base = load_profiles(Some(profiles))?
            .get(backend)
            .cloned()
            .unwrap_or_else(|| CapabilityProfile::seeded(backend));
        apply_benchmark(&base, &scored.result)
    } else {
        merge_into_profiles(backend, &scored.result, profiles)?
    };

    print!("{}", render_run(backend, &merged, &scored.result, dry_run));
    Ok(())
}

/// Score one recorded fixture and (unless `dry_run`) merge it into `profiles.toml`. Immutable:
/// builds a brand-new `ProfileSet` rather than mutating the loaded one.
fn replay_fixture(
    path: &Path,
    backend: Option<BackendId>,
    dry_run: bool,
    profiles: &Path,
) -> Result<()> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("failed to read fixture {}", path.display()))?;
    let fixture: ReplayFixture = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse fixture {}", path.display()))?;

    if let Some(requested) = backend
        && requested != fixture.backend
    {
        bail!(
            "--backend {requested} does not match fixture backend {}",
            fixture.backend
        );
    }

    let tasks = all_tasks();
    let result = score_fixture(&tasks, &fixture)?;

    let set = load_profiles(Some(profiles))?;
    let base = set
        .get(fixture.backend)
        .cloned()
        .unwrap_or_else(|| CapabilityProfile::seeded(fixture.backend));
    let merged = apply_benchmark(&base, &result);

    print!("{}", render_run(fixture.backend, &merged, &result, dry_run));

    if !dry_run {
        // Rebuild the set with the target backend replaced — never mutate the loaded one.
        let others = set
            .iter()
            .filter(|(b, _)| **b != fixture.backend)
            .map(|(_, p)| p.clone());
        let next = ProfileSet::from_profiles(others.chain(std::iter::once(merged)));
        save_profiles(&next, profiles)?;
    }
    Ok(())
}

/// Render a replay merge: the per-category scores it produced and where they land. Pure.
fn render_run(
    backend: BackendId,
    merged: &CapabilityProfile,
    result: &BenchmarkResult,
    dry_run: bool,
) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let verb = if dry_run { "would merge" } else { "merged" };
    let _ = writeln!(
        out,
        "{verb} {} bench score(s) for [{}]",
        result.scores.len(),
        style(backend).cyan()
    );
    for (category, score) in &result.scores {
        let _ = writeln!(
            out,
            "  {:<18} {:>5}  conf {:>4.2}  samples {}",
            category.as_str(),
            score.value,
            score.confidence,
            score.samples
        );
    }
    let _ = writeln!(out, "  profile source → {}", merged.source);
    out
}

/// Render the gold-bench plan: every suite task grouped by category, executing nothing. Pure.
fn render_plan() -> String {
    use std::fmt::Write as _;
    let tasks = all_tasks();
    let categories: BTreeSet<_> = tasks.iter().map(|t| t.category).collect();

    let mut out = String::new();
    let _ = writeln!(
        out,
        "gold-bench plan: {} tasks across {} categories (offline dry-run — nothing executed)",
        tasks.len(),
        categories.len()
    );
    for task in &tasks {
        let _ = writeln!(out, "  [{:<18}] {}", task.category.as_str(), task.id);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{CapabilityProfile, ProfileSource, Score, TaskCategory, load_profiles};
    use crate::types::BackendId;
    use tempfile::tempdir;

    #[test]
    fn render_show_includes_source_and_scores() {
        let set = seeded_profiles();
        let path = Path::new("/tmp/profiles.toml");
        let out = render_show(&set, path, true);

        assert!(out.contains("profiles: /tmp/profiles.toml"));
        assert!(out.contains("source=seeded"));
        assert!(out.contains("coding"));
        // Claude's seeded coding score.
        assert!(out.contains("88"));
        // The seed confidence renders to two decimals.
        assert!(out.contains("0.40"));
    }

    #[test]
    fn render_show_flags_unpersisted_defaults() {
        let set = seeded_profiles();
        let out = render_show(&set, Path::new("/tmp/absent.toml"), false);
        assert!(out.contains("seeded defaults"));
        assert!(out.contains("profile seed"));
    }

    #[test]
    fn seed_writes_then_refuses_without_force() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("profiles.toml");

        seed(&path, false).unwrap();
        assert!(path.exists());

        // A second seed without --force must refuse rather than clobber.
        let err = seed(&path, false).unwrap_err();
        assert!(format!("{err:#}").contains("--force"), "got: {err:#}");

        // With --force it succeeds.
        seed(&path, true).unwrap();
    }

    #[test]
    fn reset_clears_measured_values_back_to_seeded() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("profiles.toml");

        // Seed a benchmarked profile with real samples on disk.
        let mut profile = CapabilityProfile::seeded(BackendId::Codex);
        profile.source = ProfileSource::Benchmarked;
        profile.scores.insert(
            TaskCategory::Review,
            Score {
                value: 91,
                samples: 24,
                confidence: 0.82,
                source: ProfileSource::Benchmarked,
            },
        );
        let measured = ProfileSet::from_profiles([profile]);
        save_profiles(&measured, &path).unwrap();

        reset(&path).unwrap();

        let reloaded = load_profiles(Some(&path)).unwrap();
        let codex = reloaded.get(BackendId::Codex).expect("codex re-seeded");
        assert_eq!(codex.source, ProfileSource::Seeded);
        // Every score is back to a zero-sample seeded prior.
        for score in codex.scores.values() {
            assert_eq!(score.samples, 0);
        }
    }

    /// A minimal recorded fixture: one perfect coding outcome plus one perfect review outcome
    /// for codex, in the on-disk JSON shape that `--replay` consumes.
    const CODEX_FIXTURE: &str = r#"{
        "backend": "codex",
        "measured_at": "2026-06-30T00:00:00Z",
        "outcomes": [
            { "task_id": "coding/parse_duration", "passed": 4, "total": 4 },
            { "task_id": "review/api_handler_bug", "passed": 1, "total": 1 }
        ]
    }"#;

    fn write_fixture(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("fixture.json");
        fs::write(&path, body).unwrap();
        path
    }

    /// A minimal *raw-output* fixture for codex: one review task whose verbatim output reports no
    /// defects. The on-disk shape that `--raw-replay` re-grades through the real scorers.
    const CODEX_RAW_FIXTURE: &str = r#"{
        "backend": "codex",
        "measured_at": "2026-06-30T00:00:00Z",
        "outputs": [
            { "task_id": "review/api_handler_bug", "output": "no issues\n```json\n[]\n```" }
        ]
    }"#;

    #[tokio::test]
    async fn run_replay_scores_fixture_and_merges_into_profiles() {
        let dir = tempdir().unwrap();
        let profiles = dir.path().join("profiles.toml");
        let fixture = write_fixture(dir.path(), CODEX_FIXTURE);

        run_bench(None, Some(&fixture), None, None, false, &profiles)
            .await
            .unwrap();

        // The fixture's scores were merged and persisted.
        let reloaded = load_profiles(Some(&profiles)).unwrap();
        let codex = reloaded.get(BackendId::Codex).expect("codex measured");
        assert_eq!(codex.source, ProfileSource::Benchmarked);
        assert_eq!(codex.measured_at.as_deref(), Some("2026-06-30T00:00:00Z"));
        assert_eq!(codex.score(TaskCategory::Coding).unwrap().value, 100);
        assert_eq!(codex.score(TaskCategory::Review).unwrap().value, 100);
        assert_eq!(codex.score(TaskCategory::Coding).unwrap().samples, 1);
    }

    #[tokio::test]
    async fn run_replay_dry_run_scores_without_writing() {
        let dir = tempdir().unwrap();
        let profiles = dir.path().join("profiles.toml");
        let fixture = write_fixture(dir.path(), CODEX_FIXTURE);

        run_bench(None, Some(&fixture), None, None, true, &profiles)
            .await
            .unwrap();

        assert!(!profiles.exists(), "dry-run must not write profiles.toml");
    }

    #[tokio::test]
    async fn run_replay_rejects_backend_mismatch() {
        let dir = tempdir().unwrap();
        let profiles = dir.path().join("profiles.toml");
        let fixture = write_fixture(dir.path(), CODEX_FIXTURE);

        let err = run_bench(
            Some(BackendId::Claude),
            Some(&fixture),
            None,
            None,
            false,
            &profiles,
        )
        .await
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("does not match"),
            "got: {err:#}"
        );
        assert!(!profiles.exists());
    }

    #[tokio::test]
    async fn run_raw_replay_grades_through_judge_and_merges() {
        let dir = tempdir().unwrap();
        let profiles = dir.path().join("profiles.toml");
        let fixture = write_fixture(dir.path(), CODEX_RAW_FIXTURE);

        // The offline raw path runs judge::grade for real: an empty defect array misses every
        // embedded defect, so the Review score is 0 — and it still merges + persists.
        run_bench(None, None, Some(&fixture), None, false, &profiles)
            .await
            .unwrap();

        let reloaded = load_profiles(Some(&profiles)).unwrap();
        let codex = reloaded.get(BackendId::Codex).expect("codex measured");
        assert_eq!(codex.source, ProfileSource::Benchmarked);
        assert_eq!(codex.score(TaskCategory::Review).unwrap().value, 0);
    }

    #[tokio::test]
    async fn run_replay_and_raw_replay_are_mutually_exclusive() {
        let dir = tempdir().unwrap();
        let profiles = dir.path().join("profiles.toml");
        let fixture = write_fixture(dir.path(), CODEX_FIXTURE);

        let err = run_bench(None, Some(&fixture), Some(&fixture), None, false, &profiles)
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("mutually exclusive"),
            "got: {err:#}"
        );
    }

    #[tokio::test]
    async fn run_dry_run_without_fixture_prints_plan() {
        let dir = tempdir().unwrap();
        let profiles = dir.path().join("profiles.toml");

        // The plan path scores nothing and writes nothing.
        run_bench(None, None, None, None, true, &profiles)
            .await
            .unwrap();
        assert!(!profiles.exists());

        let plan = render_plan();
        assert!(plan.contains("gold-bench plan"));
        assert!(plan.contains("coding/parse_duration"));
    }

    #[tokio::test]
    async fn run_without_target_demands_one() {
        let dir = tempdir().unwrap();
        let profiles = dir.path().join("profiles.toml");
        let err = run_bench(None, None, None, None, false, &profiles)
            .await
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("specify a target"),
            "got: {err:#}"
        );
    }

    fn lbl(
        backend: BackendId,
        hash: &str,
        success: bool,
        weight: f32,
        ts: u64,
    ) -> crate::profile::learn::Label {
        crate::profile::learn::Label {
            backend,
            category: crate::profile::TaskCategory::Coding,
            success,
            weight,
            task_hash: Some(hash.into()),
            ts,
        }
    }

    /// Review finding M6: one exit-ok label must not outvote heavier bad verdicts (the old
    /// OR fold collapsed good1/bad4 to good).
    #[test]
    fn fold_verdicts_uses_weighted_majority_with_latest_tie_break() {
        let labels = vec![
            lbl(BackendId::Opencode, "h1", true, 0.5, 1),  // exit ok
            lbl(BackendId::Opencode, "h1", false, 3.0, 2), // human verdict: bad
            lbl(BackendId::Codex, "h1", true, 1.0, 3),
            lbl(BackendId::Codex, "h1", false, 1.0, 4), // tie by weight → latest wins
        ];
        let verdicts = fold_verdicts(&labels);
        let coding = crate::profile::TaskCategory::Coding;
        assert!(!verdicts[&("h1".to_string(), coding, BackendId::Opencode)]);
        assert!(!verdicts[&("h1".to_string(), coding, BackendId::Codex)]);

        // Review finding: a verdict must not leak across categories. The same task+backend
        // graded good as Docs must leave the Coding verdict (bad, above) untouched.
        let mut cross = labels.clone();
        cross.push(crate::profile::learn::Label {
            backend: BackendId::Opencode,
            category: crate::profile::TaskCategory::Docs,
            success: true,
            weight: 3.0,
            task_hash: Some("h1".into()),
            ts: 9,
        });
        let split = fold_verdicts(&cross);
        assert!(!split[&("h1".to_string(), coding, BackendId::Opencode)]);
        assert!(
            split[&(
                "h1".to_string(),
                crate::profile::TaskCategory::Docs,
                BackendId::Opencode
            )]
        );
    }

    /// Review finding M6: evaluation is per routing decision, not per label — five reruns
    /// of the same task score the deterministic pick once.
    #[test]
    fn replay_core_scores_each_task_category_decision_once() {
        let labels: Vec<_> = (0..5)
            .map(|i| lbl(BackendId::Opencode, "h1", true, 3.0, i))
            .collect();
        let picker: PolicyPicker = Box::new(|_| Some(BackendId::Opencode));
        let report = replay_core(&labels, &picker);
        assert_eq!(report.labels, 5);
        assert_eq!(report.decisions, 1);
        assert_eq!(report.evaluable, 1);
        assert_eq!(report.correct, 1);
    }

    /// Review finding M7: the learned/seeded pickers reproduce the deployed profile stage —
    /// within `quality_margin`, the cheaper backend wins, unlike a raw `best_for` argmax.
    #[test]
    fn profile_stage_picker_applies_the_deployed_cost_tiebreak() {
        use crate::profile::{CapabilityProfile, ProfileSet, ProfileSource, Score, TaskCategory};
        use std::collections::{HashMap, HashSet};

        let cell = |value: u8| Score {
            value,
            samples: 10,
            confidence: 0.7,
            source: ProfileSource::Learned,
        };
        let mut claude = CapabilityProfile::seeded(BackendId::Claude);
        claude.scores.insert(TaskCategory::Coding, cell(90));
        let mut opencode = CapabilityProfile::seeded(BackendId::Opencode);
        opencode.scores.insert(TaskCategory::Coding, cell(87));
        let set = ProfileSet::from_profiles([claude, opencode]);
        let available: HashSet<BackendId> = [BackendId::Claude, BackendId::Opencode].into();
        let costs: HashMap<BackendId, u8> =
            [(BackendId::Claude, 80), (BackendId::Opencode, 20)].into();

        let label = lbl(BackendId::Opencode, "h1", true, 1.0, 0);
        // Margin 5: Opencode (87, cost 20) is within reach of Claude (90, cost 80) → cheaper wins.
        let picker = profile_stage_picker(set.clone(), available.clone(), 5, costs.clone());
        assert_eq!(picker(&label), Some(BackendId::Opencode));
        // Margin 0: quality decides alone.
        let strict = profile_stage_picker(set, available, 0, costs);
        assert_eq!(strict(&label), Some(BackendId::Claude));
    }

    #[test]
    fn learn_flips_best_for_after_ten_good_labels_and_respects_dry_run() {
        use crate::profile::{ProfileSource, TaskCategory, seeded_profiles};
        use std::collections::HashSet;

        // Serialize XDG_STATE_HOME mutation with the other state-dir tests.
        let _g = crate::ask::STATE_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempdir().unwrap();
        // SAFETY: single-threaded under STATE_ENV_LOCK.
        unsafe {
            std::env::set_var("XDG_STATE_HOME", dir.path());
        }

        // Synthesize 10 human-verdict-good Opencode coding runs (seeded: Claude 88 > Opencode 66).
        let mut log = String::new();
        for i in 0..10 {
            log.push_str(&format!(
                "{{\"event\":\"run_started\",\"ts\":{i},\"run_id\":\"r-{i}\",\"pid\":1,\"kind\":\"rescue\",\"members\":[\"opencode\"],\"cwd\":\"/x\"}}\n\
                 {{\"event\":\"route_decided\",\"ts\":{i},\"run_id\":\"r-{i}\",\"backend\":\"opencode\",\"reason\":\"profile\",\"category\":\"coding\",\"task_hash\":\"h{i}\"}}\n\
                 {{\"event\":\"run_finished\",\"ts\":{i},\"run_id\":\"r-{i}\",\"status\":\"ok\"}}\n\
                 {{\"event\":\"outcome_noted\",\"ts\":{i},\"run_id\":\"r-{i}\",\"outcome\":\"good\"}}\n",
            ));
        }
        let state = dir.path().join("agentpit");
        fs::create_dir_all(&state).unwrap();
        fs::write(state.join("events.jsonl"), log).unwrap();

        let profiles = dir.path().join("profiles.toml");
        let available: HashSet<BackendId> = BackendId::ALL.iter().copied().collect();
        assert_eq!(
            seeded_profiles()
                .best_for(TaskCategory::Coding, &available)
                .unwrap()
                .0,
            BackendId::Claude,
            "precondition: Claude leads Coding in the seed"
        );

        // Dry run reports but writes nothing.
        learn(true, 5, &profiles).unwrap();
        assert!(!profiles.exists());

        learn(false, 5, &profiles).unwrap();
        let merged = load_profiles(Some(&profiles)).unwrap();
        let learned = merged.get(BackendId::Opencode).unwrap();
        assert_eq!(learned.source, ProfileSource::Learned);
        let (best, score) = merged
            .best_for(TaskCategory::Coding, &available)
            .expect("coding is scored");
        assert_eq!(best, BackendId::Opencode, "10 good labels flip the argmax");
        assert!(score.value > 88, "beta posterior beats Claude's seed");
        assert_eq!(score.samples, 10);
        assert!(score.confidence <= 0.85);

        unsafe {
            std::env::remove_var("XDG_STATE_HOME");
        }
    }
}
