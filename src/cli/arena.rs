//! `agentpit arena run | vote | leaderboard` — blind head-to-head comparison of backends on a
//! real build task, judged by a human.
//!
//! The three subcommands are deliberately separate steps rather than one blocking flow. A round
//! is N full agentic runs and takes minutes; nobody should have to sit at the terminal for it,
//! and a judgement made while impatient is a worse judgement. So `run` finishes and exits, `vote`
//! is picked up whenever the human is ready, and `leaderboard` reads the accumulated record.

use std::fs;

use anyhow::{Context, Result, bail};
use clap::Subcommand;
use console::style;

use super::cancel::{self, Nav};
use crate::arena::{
    self, Contender,
    rating::{self, Rating},
    store::{self, Round, Submission},
};
use crate::effort::Effort;
use crate::events::{LegStatus, RunKind, RunLogger};
use crate::types::BackendId;

/// How much of a patch is printed inline before the judge is pointed at the file instead.
const PATCH_PREVIEW_LINES: usize = 120;

#[derive(Subcommand, Debug)]
pub enum Action {
    /// Run one round: every contender builds the same thing in its own git worktree.
    Run {
        /// What to build. Quote multi-word tasks. Omit when using --template.
        task: Option<String>,
        /// Use a built-in probe instead (see `agentpit arena templates`). Its declared category
        /// decides which capability cell the round's votes land in.
        #[arg(long, conflicts_with = "task")]
        template: Option<String>,
        /// The subject the template works on — a path, a symptom, or a goal, depending on the
        /// template. `arena templates` says which.
        #[arg(long, requires = "template")]
        target: Option<String>,
        /// Backends to enter (comma-separated). Defaults to the `[ensemble]` members.
        #[arg(long, value_delimiter = ',')]
        contenders: Option<Vec<BackendId>>,
        /// Maximum contenders to run at once. The default is deliberately small rather than all
        /// contenders: every slot is a full agentic run and multiplies token spend and local CPU.
        #[arg(
            long,
            value_name = "N",
            default_value_t = arena::DEFAULT_CONCURRENCY
        )]
        concurrency: std::num::NonZeroUsize,
        /// Pin the model for every contender. Otherwise each uses its `[backends.<id>].model`.
        #[arg(long)]
        model: Option<String>,
        /// Pin the reasoning effort for every contender. Otherwise each uses its own default.
        #[arg(long, value_enum)]
        effort: Option<Effort>,
        #[arg(long)]
        cwd: Option<String>,
    },
    /// Judge a finished round: each pair of submissions, shown blind, one vote each.
    ///
    /// Interactive by default. `--winner`/`--loser`/`--tie` record one vote without prompting,
    /// which is how the desktop app casts them — always by BLIND LABEL (A, B, …), never by
    /// backend, so a caller cannot vote for an identity even by accident.
    Vote {
        /// Round to judge. Defaults to the most recent one.
        #[arg(long)]
        round: Option<String>,
        /// Blind label of the winner, e.g. `A`.
        #[arg(long, requires = "loser", conflicts_with = "tie")]
        winner: Option<char>,
        /// Blind label of the loser.
        #[arg(long, requires = "winner")]
        loser: Option<char>,
        /// Record the pair as too close to call, e.g. `--tie A,B`.
        #[arg(long, value_delimiter = ',', num_args = 2)]
        tie: Option<Vec<char>>,
    },
    /// Bradley–Terry standings over every vote cast so far.
    Leaderboard {
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// List the built-in probes, one per capability the matrix tracks.
    Templates {
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// List recorded rounds, newest first, with how much of each is still unjudged.
    Rounds {
        #[arg(long, default_value_t = false)]
        json: bool,
    },
    /// Apply a stored submission's patch to the working tree.
    ///
    /// A round's worktrees are gone by the time it is judged; the patch is what survives, and
    /// without this the only way to land a winner is to dig it out of the round JSON by hand.
    Apply {
        /// Round id. Defaults to the most recent one.
        round: Option<String>,
        /// Blind label to apply, e.g. `A`. Defaults to the round's vote winner.
        #[arg(long)]
        label: Option<char>,
        /// Apply even when the working tree already has changes.
        #[arg(long, default_value_t = false)]
        force: bool,
    },
    /// Show one round's submissions under their blind labels.
    Show {
        /// Round id. Defaults to the most recent one.
        round: Option<String>,
        /// Include which backend produced each submission. Off by default: the labels exist so
        /// the work can be judged without knowing whose it is.
        #[arg(long, default_value_t = false)]
        reveal: bool,
        #[arg(long, default_value_t = false)]
        json: bool,
    },
}

pub async fn run(action: Action) -> Result<()> {
    match action {
        Action::Run {
            task,
            template,
            target,
            contenders,
            concurrency,
            model,
            effort,
            cwd,
        } => {
            let task = resolve_task(task, template.as_deref(), target.as_deref())?;
            run_round(task, contenders, concurrency.get(), model, effort, cwd).await
        }
        Action::Vote {
            round,
            winner,
            loser,
            tie,
        } => match (winner, loser, tie) {
            (Some(w), Some(l), _) => vote_once(round, w, l, false),
            (_, _, Some(pair)) => vote_once(round, pair[0], pair[1], true),
            _ => vote(round),
        },
        Action::Leaderboard { json } => leaderboard(json),
        Action::Templates { json } => {
            if json {
                println!("{}", serde_json::to_string_pretty(&templates_json())?);
            } else {
                print!("{}", render_templates());
            }
            Ok(())
        }
        Action::Rounds { json } => rounds(json),
        Action::Apply {
            round,
            label,
            force,
        } => apply(round, label, force),
        Action::Show {
            round,
            reveal,
            json,
        } => show(round, reveal, json),
    }
}

/// The most recent round, or a clear error when none exist.
fn latest_round() -> Result<String> {
    store::list_rounds()
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("no arena rounds yet — run `agentpit arena run` first"))
}

/// Map a round's blind labels back to submission indices.
fn label_index(round: &Round) -> std::collections::BTreeMap<char, usize> {
    round.blind_order().into_iter().collect()
}

/// Record one vote non-interactively, addressed by blind label.
fn vote_once(round_id: Option<String>, a: char, b: char, tie: bool) -> Result<()> {
    let round_id = match round_id {
        Some(id) => id,
        None => latest_round()?,
    };
    let round = store::load_round(&round_id)?;
    let by_label = label_index(&round);
    let a = a.to_ascii_uppercase();
    let b = b.to_ascii_uppercase();
    if a == b {
        bail!("a submission cannot be compared with itself");
    }
    let resolve = |l: char| -> Result<BackendId> {
        by_label
            .get(&l)
            .map(|i| round.submissions[*i].backend)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "round {round_id} has no submission labelled {l} (labels: {})",
                    by_label.keys().collect::<String>()
                )
            })
    };
    let (first, second) = (resolve(a)?, resolve(b)?);
    let vote = store::Vote {
        round_id: round_id.clone(),
        ts: now_ms(),
        winner: (!tie).then_some(first),
        loser: (!tie).then_some(second),
        tie,
    };
    store::append_vote(&vote)?;
    emit_grades(&round);
    println!(
        "recorded: {}",
        match tie {
            true => format!("{a} / {b} tie"),
            false => format!("{a} beats {b}"),
        }
    );
    Ok(())
}

/// Apply one submission's patch to the working tree.
///
/// Refuses on a dirty tree unless forced: a patch landing on top of unrelated edits is hard to
/// unpick afterwards, and the mistake is silent. Refuses to guess a winner when the votes do not
/// name one — a tie, or an unjudged round, is not a decision.
fn apply(round_id: Option<String>, label: Option<char>, force: bool) -> Result<()> {
    let round_id = match round_id {
        Some(id) => id,
        None => latest_round()?,
    };
    let round = store::load_round(&round_id)?;
    let by_label = label_index(&round);

    let index = match label {
        Some(l) => {
            let l = l.to_ascii_uppercase();
            *by_label.get(&l).ok_or_else(|| {
                anyhow::anyhow!(
                    "round {round_id} has no submission labelled {l} (labels: {})",
                    by_label.keys().collect::<String>()
                )
            })?
        }
        None => winning_index(&round, &round_id)?,
    };
    let submission = &round.submissions[index];
    if submission.patch.trim().is_empty() {
        bail!("that submission changed nothing — there is no patch to apply");
    }

    if !force && !git(&["status", "--porcelain"])?.trim().is_empty() {
        bail!(
            "the working tree has uncommitted changes. Commit or stash them first, or pass \
             --force to apply on top of them."
        );
    }

    let staged = std::env::temp_dir().join(format!("agentpit-arena-apply-{}.patch", round_id));
    fs::write(&staged, &submission.patch)
        .with_context(|| format!("failed to stage the patch at {}", staged.display()))?;
    let result = git(&["apply", &staged.display().to_string()]);
    let _ = fs::remove_file(&staged);
    result?;

    let (added, removed) = arena::worktree::patch_size(&submission.patch);
    println!(
        "applied {} from {round_id} (+{added}/-{removed}) — {}",
        submission.backend,
        match &submission.verify {
            Some(v) => v.summary(),
            None => "no check was run".into(),
        }
    );
    if !submission.binary_files.is_empty() {
        println!(
            "  note: {} binary file(s) were omitted from the patch and are NOT applied: {}",
            submission.binary_files.len(),
            submission.binary_files.join(", ")
        );
    }
    Ok(())
}

/// The submission this round's votes chose. Errors when they did not choose one.
fn winning_index(round: &Round, round_id: &str) -> Result<usize> {
    let mut wins: std::collections::BTreeMap<BackendId, usize> = Default::default();
    for v in store::load_votes()
        .iter()
        .filter(|v| v.round_id == round_id)
    {
        if let Some(w) = v.winner {
            *wins.entry(w).or_default() += 1;
        }
    }
    let best = wins.values().copied().max().unwrap_or(0);
    if best == 0 {
        bail!("round {round_id} has no decisive vote yet — judge it first, or pass --label");
    }
    let leaders: Vec<BackendId> = wins
        .iter()
        .filter(|(_, n)| **n == best)
        .map(|(b, _)| *b)
        .collect();
    if leaders.len() > 1 {
        bail!(
            "round {round_id} is tied between {} — pass --label to choose",
            leaders
                .iter()
                .map(|b| b.as_str())
                .collect::<Vec<_>>()
                .join(" and ")
        );
    }
    round
        .submissions
        .iter()
        .position(|s| s.backend == leaders[0])
        .ok_or_else(|| anyhow::anyhow!("the winning backend has no submission in this round"))
}

fn git(args: &[&str]) -> Result<String> {
    let out = std::process::Command::new("git")
        .args(args)
        .output()
        .with_context(|| format!("failed to run git {}", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The built-in probes as data, for the desktop app's picker.
fn templates_json() -> serde_json::Value {
    serde_json::json!(
        arena::templates::ALL
            .iter()
            .map(|t| serde_json::json!({
                "id": t.id,
                "category": t.category.as_str(),
                "probes": t.probes,
                "target": t.target,
            }))
            .collect::<Vec<_>>()
    )
}

/// Every recorded round, newest first, with how much of it is still unjudged.
fn rounds(json: bool) -> Result<()> {
    let votes = store::load_votes();
    let mut out = Vec::new();
    for id in store::list_rounds() {
        let Ok(round) = store::load_round(&id) else {
            continue;
        };
        let cast = votes.iter().filter(|v| v.round_id == id).count();
        let total = round.matchups().len();
        out.push(serde_json::json!({
            "round_id": id,
            "task": round.task,
            "cwd": round.cwd,
            "contenders": round.submissions.iter().map(|s| s.backend.as_str()).collect::<Vec<_>>(),
            "judgeable": round.blind_order().len(),
            "matchups": total,
            "votes": cast,
            "pending": total.saturating_sub(cast),
        }));
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(());
    }
    if out.is_empty() {
        println!("no arena rounds yet — run `agentpit arena run` first.");
        return Ok(());
    }
    for r in &out {
        println!(
            "  {}  {}/{} judged  [{}]\n      {}",
            r["round_id"].as_str().unwrap_or(""),
            r["votes"],
            r["matchups"],
            r["contenders"]
                .as_array()
                .map(|a| a
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_default(),
            r["task"]
                .as_str()
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or(""),
        );
    }
    Ok(())
}

/// One round's submissions under their blind labels.
fn show(round_id: Option<String>, reveal: bool, json: bool) -> Result<()> {
    let round_id = match round_id {
        Some(id) => id,
        None => latest_round()?,
    };
    let round = store::load_round(&round_id)?;
    let cast = store::load_votes()
        .iter()
        .filter(|v| v.round_id == round_id)
        .count();
    let entries: Vec<serde_json::Value> = round
        .blind_order()
        .into_iter()
        .map(|(label, i)| {
            let s = &round.submissions[i];
            let (added, removed) = arena::worktree::patch_size(&s.patch);
            let mut e = serde_json::json!({
                "label": label.to_string(),
                "added": added,
                "removed": removed,
                "patch": s.patch,
                "summary": s.summary,
                "binary_files": s.binary_files,
                "verify": s.verify,
            });
            // Identity is withheld unless asked for: the labels exist so the work can be judged
            // without knowing whose it is, and a UI that received the names would have to
            // remember not to show them.
            if reveal {
                e["backend"] = serde_json::json!(s.backend.as_str());
                e["model"] = serde_json::json!(s.model);
                e["effort"] = serde_json::json!(s.effort.map(|x| x.to_string()));
            }
            e
        })
        .collect();
    let pairs: Vec<serde_json::Value> = {
        let by_index: std::collections::BTreeMap<usize, char> = round
            .blind_order()
            .into_iter()
            .map(|(l, i)| (i, l))
            .collect();
        round
            .matchups()
            .into_iter()
            .map(|(a, b)| serde_json::json!([by_index[&a].to_string(), by_index[&b].to_string()]))
            .collect()
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "round_id": round_id,
                "task": round.task,
                "cwd": round.cwd,
                "votes": cast,
                "matchups": pairs,
                "submissions": entries,
            }))?
        );
        return Ok(());
    }
    println!("\nround {round_id}\ntask: {}\n", round.task);
    for e in &entries {
        println!(
            "  {}  +{}/-{}{}",
            e["label"].as_str().unwrap_or(""),
            e["added"],
            e["removed"],
            match reveal {
                true => format!("  [{}]", e["backend"].as_str().unwrap_or("?")),
                false => String::new(),
            }
        );
    }
    Ok(())
}

/// The task text for this round: a free-text one, or a template rendered with its target. Pure so
/// the "neither was given" message is covered by a test rather than only reachable by hand.
fn resolve_task(
    task: Option<String>,
    template: Option<&str>,
    target: Option<&str>,
) -> Result<String> {
    match (task, template) {
        (Some(task), _) => Ok(task),
        (None, Some(id)) => {
            let t = arena::templates::find(id).ok_or_else(|| {
                anyhow::anyhow!("unknown template '{id}'. `agentpit arena templates` lists them.")
            })?;
            t.render(target).map_err(|e| anyhow::anyhow!(e))
        }
        (None, None) => {
            bail!("give a task, or pick a probe with --template (see `agentpit arena templates`)")
        }
    }
}

/// The built-in probes, grouped by the capability cell each one fills. Pure.
fn render_templates() -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(
        out,
        "\nBuilt-in arena probes — one per capability the matrix tracks. Each declares its own\n\
         category, so a round's votes land in the cell named here rather than one guessed from\n\
         the task text.\n"
    );
    for t in arena::templates::ALL {
        let _ = writeln!(
            out,
            "  {:<34} [{}]\n      {}",
            style(t.id).cyan(),
            t.category.as_str(),
            t.probes
        );
        if let Some(what) = t.target {
            let _ = writeln!(out, "      --target: {what}");
        }
        let _ = writeln!(out);
    }
    let _ = writeln!(
        out,
        "  agentpit arena run --template <id> --target <subject>"
    );
    out
}

async fn run_round(
    task: String,
    contenders: Option<Vec<BackendId>>,
    concurrency: usize,
    model: Option<String>,
    effort: Option<Effort>,
    cwd: Option<String>,
) -> Result<()> {
    let ctx = super::load_context()?;
    let cwd = super::resolve_cwd(cwd)?;

    let available = ctx.regs.available();
    let entered: Vec<BackendId> = contenders
        .unwrap_or_else(|| ctx.loaded.config.ensemble.default_members.clone())
        .into_iter()
        .filter(|b| available.contains(b))
        .collect();
    if entered.len() < 2 {
        bail!(
            "an arena needs at least two available contenders (got {}). Pass --contenders a,b or \
             configure [ensemble].default_members.",
            entered.len()
        );
    }

    // Each contender runs at ITS OWN model/effort unless a flag pins one for everybody: that is
    // what makes the result attributable to a (backend, model, effort) triple rather than to a
    // CLI's name. Resolved up front so the round records what it actually ran.
    let field: Vec<Contender> = entered
        .iter()
        .map(|b| Contender {
            backend: *b,
            model: crate::workflow::roles::resolve_model(
                model.as_deref(),
                None,
                ctx.loaded
                    .config
                    .backends
                    .get(b)
                    .and_then(|o| o.model.as_deref()),
            ),
            effort: crate::effort::resolve_effort(
                effort,
                None,
                ctx.loaded.config.backends.get(b).and_then(|o| o.effort),
            )
            .map(|e| e.clamp_for(*b)),
        })
        .collect();

    let cancel = tokio_util::sync::CancellationToken::new();
    super::install_ctrlc_cancel(cancel.clone());

    let logger = RunLogger::start(RunKind::Arena, &entered, &cwd);
    // The learning fold reads a run's category from its `RouteDecided` line (or re-diagnoses the
    // task text saved with it), so without this the votes would have nowhere to land. No router
    // ran — the contenders were chosen by the human — hence the "arena" reason, matching how the
    // ensemble path labels its own fan-out.
    logger.route_decided(
        entered[0],
        "arena",
        None,
        None,
        None,
        model.as_deref(),
        effort.map(|e| e.as_str()),
        &task,
    );
    let round_id = format!("arena-{}", logger.run_id());
    eprintln!(
        "{} arena round {round_id}: {} contenders, one worktree each, up to {} running at once",
        style("→").bold(),
        field.len(),
        concurrency.min(field.len())
    );

    let round = arena::run_round(
        &round_id,
        logger.run_id(),
        &task,
        &cwd,
        &field,
        &ctx.regs,
        cancel,
        concurrency,
        ctx.loaded.config.arena.verify.as_deref(),
        |c, i, n, progress| match progress {
            arena::Progress::Started => {
                eprintln!(
                    "  [{i}/{n}] {} started (model={}, effort={})",
                    c.backend,
                    c.model.as_deref().unwrap_or("CLI default"),
                    c.effort
                        .map(|e| e.to_string())
                        .unwrap_or_else(|| "CLI default".into()),
                );
            }
            arena::Progress::Finished { failed: false } => {
                eprintln!("  [{i}/{n}] {} finished", c.backend);
            }
            arena::Progress::Finished { failed: true } => {
                eprintln!("  [{i}/{n}] {} failed", c.backend);
            }
        },
    )
    .await;

    let round = match round {
        Ok(r) => r,
        Err(e) => {
            logger.finished(LegStatus::Error);
            return Err(e);
        }
    };
    logger.finished(LegStatus::Ok);

    let path = store::save_round(&round)?;
    print!("{}", render_round_summary(&round));
    println!("saved → {}", style(path.display()).dim());
    let judgeable = round.blind_order().len();
    if judgeable < 2 {
        println!(
            "{} only {judgeable} contender(s) produced changes — nothing to compare.",
            style("⚠").yellow()
        );
        return Ok(());
    }
    println!("judge it with: agentpit arena vote");
    Ok(())
}

/// The round as it stands: who produced what, and who is eligible to be judged. Pure.
fn render_round_summary(round: &Round) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "\nround {} — {}", round.round_id, round.task);
    for s in &round.submissions {
        let (added, removed) = arena::worktree::patch_size(&s.patch);
        let state = match (&s.error, s.judgeable()) {
            (Some(e), _) => format!("failed: {e}"),
            (None, false) => "no changes".to_string(),
            (None, true) => format!("+{added}/-{removed}"),
        };
        let check = match &s.verify {
            Some(v) => format!("  {}", v.summary()),
            None => String::new(),
        };
        let _ = writeln!(out, "  [{}] {state}{check}", s.backend);
    }
    out
}

fn vote(round_id: Option<String>) -> Result<()> {
    let round_id = match round_id {
        Some(id) => id,
        None => store::list_rounds().into_iter().next().ok_or_else(|| {
            anyhow::anyhow!("no arena rounds yet — run `agentpit arena run` first")
        })?,
    };
    let round = store::load_round(&round_id)?;
    let labels: std::collections::BTreeMap<usize, char> = round
        .blind_order()
        .into_iter()
        .map(|(l, i)| (i, l))
        .collect();
    let matchups = round.matchups();
    if matchups.is_empty() {
        bail!("round {round_id} has fewer than two judgeable submissions");
    }

    println!("\n{}", style(format!("round {round_id}")).bold());
    println!("task: {}\n", round.task);
    println!(
        "{}",
        style("Submissions are anonymous until every vote is in.").dim()
    );

    let mut cast = 0usize;
    for (a, b) in &matchups {
        let (sa, sb) = (&round.submissions[*a], &round.submissions[*b]);
        let (la, lb) = (labels[a], labels[b]);
        print_submission(la, sa);
        print_submission(lb, sb);

        let choice = cancel::prompt(
            cliclack::select(format!("Which is the better work — {la} or {lb}?"))
                .item(Some(true), format!("{la}"), "")
                .item(Some(false), format!("{lb}"), "")
                .item(None, "too close to call", "recorded as a tie")
                .interact(),
        )?;
        let Nav::Value(pick) = choice else {
            println!("(stopped — {cast} vote(s) recorded)");
            break;
        };

        let vote = match pick {
            Some(true) => store::Vote {
                round_id: round_id.clone(),
                ts: now_ms(),
                winner: Some(sa.backend),
                loser: Some(sb.backend),
                tie: false,
            },
            Some(false) => store::Vote {
                round_id: round_id.clone(),
                ts: now_ms(),
                winner: Some(sb.backend),
                loser: Some(sa.backend),
                tie: false,
            },
            None => store::Vote {
                round_id: round_id.clone(),
                ts: now_ms(),
                winner: None,
                loser: None,
                tie: true,
            },
        };
        store::append_vote(&vote)?;
        cast += 1;
    }

    if cast > 0 {
        // The reveal comes only after voting, never between comparisons: seeing that A was your
        // usual favourite would steer every remaining pick in the same round.
        println!("\n{}", style("reveal").bold());
        for (label, i) in round.blind_order() {
            let s = &round.submissions[i];
            println!("  {label} = {}", describe(s));
        }
        emit_grades(&round);
    }
    println!("\n{cast} vote(s) recorded. `agentpit arena leaderboard` for the standings.");
    Ok(())
}

/// Feed this round's votes into the ordinary learning fold as `MemberGraded` labels.
///
/// A human's head-to-head verdict is the strongest signal agentpit can get, but it is emitted
/// through the SAME channel as every other grade rather than as a new privileged score: the
/// sample gate and the `benchmarked > learned` rule still apply. A pile of arena votes should
/// move the learned scores, not silently outrank a measured benchmark.
fn emit_grades(round: &Round) {
    let votes: Vec<store::Vote> = store::load_votes()
        .into_iter()
        .filter(|v| v.round_id == round.round_id)
        .collect();
    if votes.is_empty() {
        return;
    }
    let table = rating::rate(&arena::pairs(&votes));
    if table.is_empty() {
        return;
    }
    let logger = RunLogger::resume(&round.run_id);
    for (rank, r) in table.iter().enumerate() {
        logger.member_graded(r.backend, r.score, Some((rank + 1) as u8));
    }
}

fn describe(s: &Submission) -> String {
    let variant = match (&s.model, s.effort) {
        (None, None) => String::new(),
        (m, e) => format!(
            " ({} / {})",
            m.as_deref().unwrap_or("CLI default"),
            e.map(|e| e.to_string())
                .unwrap_or_else(|| "CLI default".into())
        ),
    };
    format!("{}{variant}", s.backend)
}

fn print_submission(label: char, s: &Submission) {
    let (added, removed) = arena::worktree::patch_size(&s.patch);
    println!(
        "\n{} {}",
        style(format!("── {label} ──")).cyan().bold(),
        style(format!("+{added}/-{removed}")).dim()
    );
    // The check's verdict, next to the diff. A red check is a fact for the judge to weigh, not
    // a disqualification — see `arena::verify`.
    if let Some(v) = &s.verify {
        let line = format!("{} — {}", v.summary(), v.command);
        println!(
            "{}",
            match v.passed {
                true => style(line).green(),
                false => style(line).yellow(),
            }
        );
        if !v.passed && !v.output.is_empty() {
            println!("{}", style(clamp_lines(&v.output, 12)).dim());
        }
    }
    if !s.binary_files.is_empty() {
        println!(
            "{}",
            style(format!(
                "({} binary file(s) omitted: {})",
                s.binary_files.len(),
                s.binary_files.join(", ")
            ))
            .dim()
        );
    }
    if !s.summary.is_empty() {
        println!("{}", style(clamp_lines(&s.summary, 12)).dim());
    }
    println!("{}", clamp_lines(&s.patch, PATCH_PREVIEW_LINES));
}

/// First `max` lines, with an explicit marker when there was more. Silently truncating would let
/// a judge compare a whole small patch against the opening of a large one without knowing it.
fn clamp_lines(text: &str, max: usize) -> String {
    let total = text.lines().count();
    if total <= max {
        return text.to_string();
    }
    let head: Vec<&str> = text.lines().take(max).collect();
    format!(
        "{}\n… {} more line(s) not shown",
        head.join("\n"),
        total - max
    )
}

fn leaderboard(json: bool) -> Result<()> {
    let votes = store::load_votes();
    let decisive = arena::pairs(&votes);
    let table = rating::rate(&decisive);
    if json {
        let rows: Vec<serde_json::Value> = table
            .iter()
            .map(|r| {
                serde_json::json!({
                    "backend": r.backend.as_str(),
                    "score": r.score,
                    "low": r.low,
                    "high": r.high,
                    "wins": r.wins,
                    "losses": r.losses,
                    "provisional": r.provisional(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "votes": votes.len(),
                "ties": votes.iter().filter(|v| v.tie).count(),
                "min_comparisons": rating::MIN_COMPARISONS,
                "standings": rows,
            }))?
        );
        return Ok(());
    }
    if table.is_empty() {
        println!("no arena votes yet — run `agentpit arena run` then `agentpit arena vote`.");
        return Ok(());
    }
    print!("{}", render_leaderboard(&table, votes.len()));
    Ok(())
}

/// The standings. Pure so the wording of the provisional warning is testable.
fn render_leaderboard(table: &[Rating], total_votes: usize) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let ties = total_votes - table.iter().map(|r| r.wins as usize).sum::<usize>();
    let _ = writeln!(
        out,
        "\narena — {total_votes} vote(s), {ties} tie(s)\n  {:<14} {:>5}  {:>11}  {:>7}",
        "contender", "score", "90% range", "record"
    );
    for r in table {
        let _ = writeln!(
            out,
            "  {:<14} {:>5}  {:>11}  {:>7}{}",
            r.backend.as_str(),
            r.score,
            format!("{}–{}", r.low, r.high),
            format!("{}-{}", r.wins, r.losses),
            match r.provisional() {
                true => "  provisional",
                false => "",
            }
        );
    }
    if table.iter().any(Rating::provisional) {
        let _ = writeln!(
            out,
            "\n  provisional = fewer than {} comparisons, or a range wide enough that the order \
             could flip.\n  These are rankings from a handful of votes, not measurements.",
            rating::MIN_COMPARISONS
        );
    }
    out
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Debug, Parser)]
    struct ArenaArgs {
        #[command(subcommand)]
        action: Action,
    }

    fn submission(backend: BackendId, patch: &str, error: Option<&str>) -> Submission {
        Submission {
            backend,
            model: None,
            effort: None,
            patch: patch.into(),
            binary_files: Vec::new(),
            summary: String::new(),
            error: error.map(str::to_string),
            verify: None,
        }
    }

    #[test]
    fn a_template_renders_into_the_task_and_free_text_still_wins() {
        let rendered =
            resolve_task(None, Some("refactor/untangle"), Some("src/router.rs")).unwrap();
        assert!(rendered.starts_with("CATEGORY: refactor"), "{rendered}");
        assert!(rendered.contains("src/router.rs"));
        // An explicit task is used verbatim — no declaration is bolted onto it, because the
        // caller did not say what it is and guessing would defeat the point of the marker.
        assert_eq!(
            resolve_task(Some("do a thing".into()), None, None).unwrap(),
            "do a thing"
        );
    }

    #[test]
    fn an_unknown_template_and_a_bare_run_both_say_what_to_do() {
        let err = format!(
            "{:#}",
            resolve_task(None, Some("nope/nope"), None).unwrap_err()
        );
        assert!(err.contains("arena templates"), "{err}");
        let err = format!("{:#}", resolve_task(None, None, None).unwrap_err());
        assert!(err.contains("--template"), "{err}");
    }

    #[test]
    fn run_concurrency_defaults_to_two_and_rejects_zero() {
        let parsed = ArenaArgs::try_parse_from(["arena", "run", "do work"]).unwrap();
        let Action::Run { concurrency, .. } = parsed.action else {
            panic!("expected arena run")
        };
        assert_eq!(concurrency, arena::DEFAULT_CONCURRENCY);

        let error = ArenaArgs::try_parse_from(["arena", "run", "do work", "--concurrency", "0"])
            .unwrap_err();
        assert!(error.to_string().contains("non-zero"), "{error}");
    }

    #[test]
    fn the_listing_names_every_probe_and_the_cell_it_fills() {
        let out = render_templates();
        for t in arena::templates::ALL {
            assert!(out.contains(t.id), "{} missing from the listing", t.id);
            assert!(out.contains(t.category.as_str()), "{}", t.id);
        }
    }

    #[test]
    fn round_summary_separates_failed_from_empty_from_real_work() {
        let round = Round {
            round_id: "r1".into(),
            run_id: "run".into(),
            task: "add a flag".into(),
            cwd: "/tmp".into(),
            submissions: vec![
                submission(BackendId::Claude, "+++ b/x\n+one\n+two\n-old\n", None),
                submission(BackendId::Codex, "", None),
                submission(BackendId::Opencode, "", Some("timed out")),
            ],
        };
        let out = render_round_summary(&round);
        assert!(out.contains("[claude] +2/-1"), "{out}");
        // "produced nothing" and "crashed" are different facts and must not both read as a loss.
        assert!(out.contains("[codex] no changes"), "{out}");
        assert!(out.contains("[opencode] failed: timed out"), "{out}");
    }

    #[test]
    fn a_truncated_patch_says_so() {
        let long = (0..200)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let shown = clamp_lines(&long, 10);
        assert_eq!(shown.lines().count(), 11, "10 lines plus the marker");
        assert!(shown.contains("190 more line(s) not shown"), "{shown}");
        // Short input is passed through untouched — no marker to mislead the judge.
        assert_eq!(clamp_lines("a\nb", 10), "a\nb");
    }

    /// A red check is information for the judge, never a disqualification: `judgeable` is about
    /// whether there is work to compare, and a failing build is very much work to compare.
    #[test]
    fn a_failing_check_does_not_remove_a_submission_from_judging() {
        let failed = Submission {
            verify: Some(crate::arena::verify::VerifyOutcome {
                command: "cargo test".into(),
                passed: false,
                exit_code: Some(101),
                output: "FAILED".into(),
            }),
            ..submission(BackendId::Codex, "+ real work", None)
        };
        assert!(failed.judgeable());
        let round = Round {
            round_id: "r1".into(),
            run_id: "run".into(),
            task: "t".into(),
            cwd: "/tmp".into(),
            submissions: vec![failed, submission(BackendId::Claude, "+ other work", None)],
        };
        assert_eq!(round.blind_order().len(), 2);
        assert_eq!(round.matchups().len(), 1);
        // And the summary states it rather than hiding it.
        assert!(
            render_round_summary(&round).contains("check failed (exit 101)"),
            "{}",
            render_round_summary(&round)
        );
    }

    #[test]
    fn the_leaderboard_marks_a_thin_record_provisional_and_explains_why() {
        let thin = rating::rate(&[rating::Pair {
            winner: BackendId::Codex,
            loser: BackendId::Claude,
        }]);
        let out = render_leaderboard(&thin, 1);
        assert!(out.contains("provisional"), "{out}");
        assert!(out.contains("not measurements"), "{out}");
    }

    #[test]
    fn ties_are_counted_in_the_header_rather_than_dropped() {
        let table = rating::rate(&[rating::Pair {
            winner: BackendId::Codex,
            loser: BackendId::Claude,
        }]);
        // Three votes were cast, one was decisive: the other two were ties and the header has
        // to say so, else the record looks more decisive than the judge was.
        let out = render_leaderboard(&table, 3);
        assert!(out.contains("3 vote(s), 2 tie(s)"), "{out}");
    }
}
