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

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use console::style;
use tokio_util::sync::CancellationToken;

use crate::profile::bench::{
    RawFixture, RawScored, ReplayFixture, all_tasks, merge_into_profiles, run_live, score_fixture,
    score_raw,
};
use crate::events::{LegStatus, RunKind, RunLogger, output_streamer};
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

    let _ = writeln!(
        out,
        "  {:<18} {:>5}  {:>5}  {:>7}",
        "category", "score", "conf", "samples"
    );
    for (category, score) in &profile.scores {
        let _ = writeln!(
            out,
            "  {:<18} {:>5}  {:>5.2}  {:>7}",
            category.as_str(),
            score.value,
            score.confidence,
            score.samples
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
}
