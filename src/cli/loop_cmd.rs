//! `agentpit loop` — start, watch and steer blueprint loops (docs/workspace-loop-design.md).
//!
//! A thin client: every verb goes through the daemon (`loop_*`) or the loop's runner
//! (`loop_op`, `loop_attach`). A finished loop has no runner; `show` and `watch` read its
//! journal from disk instead.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use agentpit_events::loops::*;
use agentpit_events::wire::{
    BlueprintSource, CODE_UNSUPPORTED, Event, FEATURE_LOOPS, Frame, LoopRow, RequestBody, Response,
    ResponseData,
};
use anyhow::{Context, Result, anyhow, bail};
use clap::Subcommand;
use console::style;

use crate::daemon::client::{Conn, connect_daemon};

#[derive(Debug, Subcommand)]
pub enum Action {
    /// Start a loop from a blueprint (a path to a .json file, or a name found in
    /// .agentpit/blueprints/ or ~/.config/agentpit/blueprints/).
    Start {
        blueprint: String,
        /// An input, as name=value (repeatable).
        #[arg(long = "input", value_name = "NAME=VALUE")]
        inputs: Vec<String>,
        /// Shorthand for --input goal=<TEXT>.
        #[arg(long)]
        goal: Option<String>,
        #[arg(long)]
        title: Option<String>,
        /// Directory the loop works in (default: the current directory).
        #[arg(long)]
        cwd: Option<String>,
        /// Create the loop without starting it (start it later with `agentpit loop resume`).
        #[arg(long)]
        no_start: bool,
        /// Follow the loop after starting it.
        #[arg(long)]
        watch: bool,
        #[arg(long)]
        json: bool,
    },
    /// List loops (unfinished only unless --all).
    Ls {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
        /// Keep running and print each loop again whenever it changes (one line, or one
        /// JSON object per line with --json).
        #[arg(long)]
        watch: bool,
    },
    /// Show one loop: stage, agents, iterations, open gates, budget.
    Show {
        /// Loop id (a unique prefix or suffix is enough).
        #[arg(name = "loop")]
        loop_ref: String,
        #[arg(long)]
        json: bool,
    },
    /// Follow a loop's records as they are written (from the start).
    Watch {
        #[arg(name = "loop")]
        loop_ref: String,
        /// Also stream live agent output.
        #[arg(long)]
        output: bool,
    },
    /// Pause: nothing new starts (--cancel also cancels running steps; they re-run on
    /// resume).
    Pause {
        #[arg(name = "loop")]
        loop_ref: String,
        #[arg(long)]
        cancel: bool,
    },
    /// Resume a paused loop (or start one created with --no-start).
    Resume {
        #[arg(name = "loop")]
        loop_ref: String,
    },
    /// Stop the loop (after running steps finish, or at once with --cancel).
    Stop {
        #[arg(name = "loop")]
        loop_ref: String,
        #[arg(long)]
        cancel: bool,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Answer a gate.
    Gate {
        #[arg(name = "loop")]
        loop_ref: String,
        gate: String,
        option: String,
        #[arg(long)]
        comment: Option<String>,
    },
    /// Tell the next agent step (of --target, or any) something.
    Instruct {
        #[arg(name = "loop")]
        loop_ref: String,
        text: String,
        #[arg(long)]
        target: Option<String>,
    },
    /// Cancel one running agent/check step.
    CancelStep {
        #[arg(name = "loop")]
        loop_ref: String,
        step: String,
    },
    /// Change the budget (unset flags keep their value).
    Budget {
        #[arg(name = "loop")]
        loop_ref: String,
        #[arg(long)]
        max_steps: Option<u32>,
        #[arg(long)]
        max_active_secs: Option<u64>,
        #[arg(long)]
        max_parallel: Option<u32>,
    },
    /// Validate a blueprint file (no daemon needed).
    Validate {
        file: PathBuf,
        #[arg(long)]
        json: bool,
    },
}

const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn run(action: Action) -> Result<()> {
    match action {
        Action::Start {
            blueprint,
            inputs,
            goal,
            title,
            cwd,
            no_start,
            watch,
            json,
        } => start(blueprint, inputs, goal, title, cwd, !no_start, watch, json).await,
        Action::Ls { all, json, watch } => {
            if watch {
                watch_list(all, json).await
            } else {
                list(all, json).await
            }
        }
        Action::Show { loop_ref, json } => show(&resolve_loop(&loop_ref)?, json).await,
        Action::Watch { loop_ref, output } => watch(&resolve_loop(&loop_ref)?, output).await,
        Action::Pause { loop_ref, cancel } => {
            let mode = if cancel {
                PauseMode::Cancel
            } else {
                PauseMode::Drain
            };
            op(&resolve_loop(&loop_ref)?, LoopOp::Pause { mode }).await
        }
        Action::Resume { loop_ref } => {
            let loop_id = resolve_loop(&loop_ref)?;
            // A loop created with --no-start is started, not resumed.
            let created = loop_dir(&loop_id)
                .and_then(|d| read_loop(&d).ok())
                .is_some_and(|(s, _)| s.status == LoopStatus::Created);
            let op_kind = if created {
                LoopOp::Start
            } else {
                LoopOp::Resume
            };
            op(&loop_id, op_kind).await
        }
        Action::Stop {
            loop_ref,
            cancel,
            reason,
        } => {
            let mode = if cancel {
                StopMode::Cancel
            } else {
                StopMode::Graceful
            };
            op(&resolve_loop(&loop_ref)?, LoopOp::Stop { mode, reason }).await
        }
        Action::Gate {
            loop_ref,
            gate,
            option,
            comment,
        } => {
            op(
                &resolve_loop(&loop_ref)?,
                LoopOp::ResolveGate {
                    gate_id: gate,
                    option,
                    comment,
                },
            )
            .await
        }
        Action::Instruct {
            loop_ref,
            text,
            target,
        } => op(&resolve_loop(&loop_ref)?, LoopOp::Instruct { text, target }).await,
        Action::CancelStep { loop_ref, step } => {
            op(
                &resolve_loop(&loop_ref)?,
                LoopOp::CancelStep { step_id: step },
            )
            .await
        }
        Action::Budget {
            loop_ref,
            max_steps,
            max_active_secs,
            max_parallel,
        } => {
            op(
                &resolve_loop(&loop_ref)?,
                LoopOp::SetBudget {
                    max_steps,
                    max_active_secs,
                    max_parallel,
                },
            )
            .await
        }
        Action::Validate { file, json } => validate_file(&file, json),
    }
}

// ---------------------------------------------------------------------------------------
// Plumbing.

/// A daemon connection that negotiated loops (so an older daemon is caught up front).
async fn daemon() -> Result<Conn> {
    let mut conn = connect_daemon(true).await?;
    let hello = conn
        .request_with_timeout(
            RequestBody::Hello {
                proto: agentpit_events::wire::PROTO_VERSION,
                features: vec![FEATURE_LOOPS.into()],
                client: Some(client_id()),
            },
            CONTROL_TIMEOUT,
        )
        .await?;
    match hello {
        ResponseData::Hello { features, .. } if features.iter().any(|f| f == FEATURE_LOOPS) => {
            Ok(conn)
        }
        _ => bail!(
            "the running daemon predates blueprint loops; run `agentpit daemon stop` and retry \
             so it restarts on this version"
        ),
    }
}

fn client_id() -> String {
    format!("agentpit/{}", env!("CARGO_PKG_VERSION"))
}

/// A response as a result, keeping the server's message (and code) on failure.
fn ok(resp: Response) -> Result<ResponseData> {
    if resp.ok {
        return Ok(resp.data.unwrap_or(ResponseData::Unit));
    }
    let msg = resp.error.unwrap_or_else(|| "request failed".into());
    if resp.code.as_deref() == Some(CODE_UNSUPPORTED) {
        bail!("{msg} (run `agentpit daemon stop` so it restarts on this version)");
    }
    Err(anyhow!(msg))
}

/// The runner socket of an unfinished loop, or `None` when it has finished.
/// One request, bounded (design §11: every client request carries a timeout).
async fn ask(conn: &mut Conn, body: RequestBody) -> Result<Response> {
    match tokio::time::timeout(CONTROL_TIMEOUT, conn.request_raw(body)).await {
        Ok(resp) => resp,
        Err(_) => bail!(
            "no answer within {}s; check `agentpit daemon status`, or run \
             `agentpit daemon stop` and retry",
            CONTROL_TIMEOUT.as_secs()
        ),
    }
}

/// Where a loop can be reached.
enum Reach {
    /// Its live runner.
    Runner(PathBuf),
    /// No runner will serve it (finished, or written by a newer agentpit): read it from
    /// disk. The message says why.
    Disk(String),
}

async fn reach(loop_id: &str) -> Result<Reach> {
    let mut d = daemon().await?;
    let resp = ask(
        &mut d,
        RequestBody::LoopEnsure {
            loop_id: loop_id.into(),
        },
    )
    .await?;
    if !resp.ok
        && matches!(
            resp.code.as_deref(),
            Some(c) if c == ErrorCode::Gone.as_str() || c == ErrorCode::ReadOnly.as_str()
        )
    {
        return Ok(Reach::Disk(resp.error.unwrap_or_default()));
    }
    match ok(resp)? {
        ResponseData::LoopRunner { socket, .. } => Ok(Reach::Runner(PathBuf::from(socket))),
        other => bail!("unexpected answer to loop_ensure: {other:?}"),
    }
}

/// Resolve a full loop id, or a unique prefix / suffix of one.
fn resolve_loop(arg: &str) -> Result<String> {
    if is_valid_loop_id(arg) {
        return Ok(arg.to_string());
    }
    let needle = arg.trim_start_matches("lp-");
    if needle.is_empty() {
        bail!("give a loop id (see `agentpit loop ls --all`)");
    }
    let ids: Vec<String> = std::fs::read_dir(loops_dir())
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| is_valid_loop_id(n))
                .filter(|n| n[3..].starts_with(needle) || n.ends_with(needle))
                .collect()
        })
        .unwrap_or_default();
    match ids.as_slice() {
        [one] => Ok(one.clone()),
        [] => bail!("no loop matches {arg:?} (see `agentpit loop ls --all`)"),
        many => bail!(
            "{arg:?} matches {} loops ({}…); give more characters",
            many.len(),
            many.iter()
                .take(3)
                .map(|s| short(s))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// The last 8 hex digits: enough to tell loops apart on a board.
fn short(loop_id: &str) -> &str {
    &loop_id[loop_id.len().saturating_sub(8)..]
}

async fn op(loop_id: &str, op: LoopOp) -> Result<()> {
    let socket = match reach(loop_id).await? {
        Reach::Runner(socket) => socket,
        Reach::Disk(why) => bail!("{why}"),
    };
    let mut conn = Conn::connect(&socket).await?;
    let name = op.name();
    let resp = ask(
        &mut conn,
        RequestBody::LoopOp(OpRequest {
            loop_id: loop_id.into(),
            op_id: new_op_id(),
            expect_seq: None,
            op,
        }),
    )
    .await?;
    match ok(resp)? {
        ResponseData::OpResult(r) => {
            let what = match r.outcome {
                OpOutcome::Noop => "nothing to do (already in that state)".to_string(),
                OpOutcome::Duplicate => "already applied".to_string(),
                _ => match r.result.as_ref().and_then(|v| v.get("instruction_id")) {
                    Some(id) => format!("queued as {}", id.as_str().unwrap_or_default()),
                    None => "done".to_string(),
                },
            };
            println!("{name}: {what}");
            Ok(())
        }
        other => bail!("unexpected answer: {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------
// start / ls / show.

#[allow(clippy::too_many_arguments)]
async fn start(
    blueprint: String,
    inputs: Vec<String>,
    goal: Option<String>,
    title: Option<String>,
    cwd: Option<String>,
    start: bool,
    follow: bool,
    json: bool,
) -> Result<()> {
    let cwd = super::common::resolve_cwd(cwd)?;
    let mut given = BTreeMap::new();
    for kv in inputs {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| anyhow!("--input takes NAME=VALUE, got {kv:?}"))?;
        given.insert(k.trim().to_string(), v.to_string());
    }
    if let Some(goal) = goal {
        given.insert("goal".into(), goal);
    }
    let path = Path::new(&blueprint);
    let source = if path.is_file() || blueprint.contains('/') || blueprint.ends_with(".json") {
        let full = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        BlueprintSource::Path {
            path: full.display().to_string(),
        }
    } else {
        BlueprintSource::Named { name: blueprint }
    };
    let mut d = daemon().await?;
    let resp = ask(
        &mut d,
        RequestBody::LoopStart {
            op_id: new_op_id(),
            blueprint: source,
            inputs: given,
            cwd: cwd.display().to_string(),
            title,
            start,
            origin: Some(Origin {
                surface: Surface::Cli,
                client: Some(client_id()),
                session_id: None,
            }),
        },
    )
    .await?;
    if !resp.ok
        && let Some(diags) = resp.details.as_ref().and_then(|d| d.get("diagnostics"))
    {
        let diags: Vec<Diagnostic> = serde_json::from_value(diags.clone()).unwrap_or_default();
        print_diagnostics(&diags);
    }
    let ResponseData::LoopStarted { loop_id, .. } = ok(resp)? else {
        bail!("unexpected answer to loop_start");
    };
    if json {
        println!("{}", serde_json::json!({ "loop_id": loop_id }));
    } else {
        println!(
            "{} {loop_id}  (follow: agentpit loop watch {})",
            style("started").green(),
            short(&loop_id)
        );
    }
    if follow {
        watch(&loop_id, false).await?;
    }
    Ok(())
}

async fn list(all: bool, json: bool) -> Result<()> {
    let mut d = daemon().await?;
    let resp = ask(
        &mut d,
        RequestBody::LoopList {
            include_terminal: all,
            limit: None,
        },
    )
    .await?;
    let ResponseData::Loops { loops } = ok(resp)? else {
        bail!("unexpected answer to loop_list");
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&loops)?);
        return Ok(());
    }
    if loops.is_empty() {
        println!(
            "no {}loops (start one with `agentpit loop start <blueprint>`)",
            if all { "" } else { "unfinished " }
        );
        return Ok(());
    }
    for row in &loops {
        println!("{}", row_line(row));
    }
    Ok(())
}

fn row_line(row: &LoopRow) -> String {
    let s = &row.summary;
    format!(
        "{}  {:<22} {:<28} {}{}",
        short(&s.loop_id),
        status_label(s),
        clamp_text(&s.title, 28),
        stage(s),
        if row.runner == "live" || s.status.is_terminal() {
            String::new()
        } else {
            format!("  {}", style("(parked)").dim())
        }
    )
}

/// `ls --watch`: the rows now, then every change as it happens (daemon `loop_watch`).
async fn watch_list(all: bool, json: bool) -> Result<()> {
    let mut d = daemon().await?;
    let resp = ask(
        &mut d,
        RequestBody::LoopWatch {
            include_terminal: all,
        },
    )
    .await?;
    let ResponseData::Loops { loops } = ok(resp)? else {
        bail!("unexpected answer to loop_watch");
    };
    let print = |row: &LoopRow| {
        if json {
            println!("{}", serde_json::to_string(row).unwrap_or_default());
        } else {
            println!("{}", row_line(row));
        }
    };
    for row in &loops {
        print(row);
    }
    loop {
        match d.recv_frame().await {
            Ok(Frame::Event(Event::LoopRow { row })) => print(&row),
            Ok(Frame::Event(Event::LoopGone { loop_id })) => {
                if json {
                    println!("{}", serde_json::json!({ "gone": loop_id }));
                } else {
                    println!("{}  {}", short(&loop_id), style("deleted").dim());
                }
            }
            Ok(_) => {}
            Err(_) => bail!("the daemon went away; run `agentpit loop ls --watch` again"),
        }
    }
}

fn status_label(s: &LoopSummary) -> String {
    let label = if s.waiting {
        format!("waiting ({} gate)", s.open_gate_count)
    } else {
        s.status.to_string()
    };
    match s.status {
        LoopStatus::Succeeded => style(label).green().to_string(),
        LoopStatus::Failed => style(label).red().to_string(),
        LoopStatus::Cancelled | LoopStatus::Paused => style(label).yellow().to_string(),
        _ if s.waiting => style(label).magenta().to_string(),
        _ => style(label).cyan().to_string(),
    }
}

/// "which stage, which agent".
fn stage(s: &LoopSummary) -> String {
    let mut parts: Vec<String> = s
        .active
        .iter()
        .map(|a| {
            let who = a
                .assignee
                .as_ref()
                .map(|x| match &x.role {
                    Some(r) => format!(" {r}@{}", x.backend),
                    None => format!(" {}", x.backend),
                })
                .unwrap_or_default();
            format!("{}{}", a.step_id, who)
        })
        .collect();
    parts.extend(
        s.iterations
            .iter()
            .filter(|i| i.open)
            .map(|i| format!("{} {}/{}", i.repeat, i.n, i.max)),
    );
    if let Some(f) = &s.finish {
        parts.push(match &f.node {
            Some(n) => format!("{} at {n}", f.reason),
            None => f.reason.to_string(),
        });
    }
    parts.join(", ")
}

async fn show(loop_id: &str, json: bool) -> Result<()> {
    // The journal is the truth and is on this machine: folding it needs no runner, so
    // looking at a parked loop does not wake it.
    let summary = read_summary(loop_id)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
        return Ok(());
    }
    let s = &summary;
    println!("{}  {}", style(&s.loop_id).bold(), s.title);
    println!(
        "  blueprint  {} ({})",
        s.blueprint.name,
        style(&s.blueprint.rev).dim()
    );
    println!("  status     {}", status_label(s));
    let now = stage(s);
    if !now.is_empty() {
        println!("  now        {now}");
    }
    for g in &s.open_gates {
        let options: Vec<&str> = g.options.iter().map(|o| o.id.as_str()).collect();
        println!(
            "  {} {} [{}] {}",
            style("gate").magenta(),
            g.gate_id,
            g.kind,
            g.prompt
        );
        println!(
            "             answer: agentpit loop gate {} {} <{}>",
            short(&s.loop_id),
            g.gate_id,
            options.join("|")
        );
    }
    if s.pending_instructions > 0 {
        println!("  pending    {} instruction(s)", s.pending_instructions);
    }
    println!(
        "  budget     {}/{} steps, {}s/{}s compute, {} at once",
        s.usage.steps,
        s.budget.max_steps,
        s.usage.active_ms / 1000,
        s.budget.max_active_secs,
        s.budget.max_parallel
    );
    if !s.writable {
        println!(
            "  {}",
            style("read-only for this agentpit (written by a newer version)").yellow()
        );
    }
    Ok(())
}

fn loop_dir_of(loop_id: &str) -> Result<PathBuf> {
    loop_dir(loop_id).ok_or_else(|| anyhow!("{loop_id} is not a loop id"))
}

fn read_summary(loop_id: &str) -> Result<LoopSummary> {
    let (state, _) = read_loop(&loop_dir_of(loop_id)?).context("read the loop journal")?;
    state
        .summary()
        .ok_or_else(|| anyhow!("{loop_id} has no loop_created record"))
}

// ---------------------------------------------------------------------------------------
// watch.

async fn watch(loop_id: &str, output: bool) -> Result<()> {
    let socket = match reach(loop_id).await? {
        Reach::Runner(socket) => socket,
        // Finished (or not ours to run): the journal on disk is the whole story.
        Reach::Disk(_) => {
            let (_, scan) = read_loop(&loop_dir_of(loop_id)?)?;
            let mut state = LoopState::default();
            for r in &scan.records {
                state.apply(r);
                print_record(loop_id, &state, r);
            }
            return Ok(());
        }
    };
    let mut conn = Conn::connect(&socket).await?;
    let resp = ask(
        &mut conn,
        RequestBody::LoopAttach {
            since: None,
            chunks: output,
        },
    )
    .await?;
    ok(resp)?;
    let mut state = LoopState::default();
    let mut at_line_start = true;
    loop {
        let frame = match conn.recv_frame().await {
            Ok(f) => f,
            Err(_) if state.status.is_terminal() => return Ok(()),
            Err(_) => bail!(
                "the runner went away (seq {}); `agentpit loop watch {}` resumes",
                state.head_seq,
                short(loop_id)
            ),
        };
        match frame {
            Frame::Event(Event::LoopRecord { rec, .. }) => {
                let Ok(r) = decode_line(&serde_json::to_string(&rec)?) else {
                    continue;
                };
                state.apply(&r);
                if !at_line_start {
                    println!();
                    at_line_start = true;
                }
                print_record(loop_id, &state, &r);
            }
            Frame::Event(Event::LoopChunk { text, .. }) => {
                print!("{}", style(&text).dim());
                at_line_start = text.ends_with('\n');
            }
            _ => {}
        }
    }
}

fn print_record(loop_id: &str, state: &LoopState, r: &LoadedRecord) {
    let Some(line) = describe(loop_id, state, r) else {
        return;
    };
    println!("{} {line}", style(format!("{:>4}", r.seq)).dim());
}

fn secs(ms: u64) -> String {
    if ms < 10_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        format!("{}s", ms / 1000)
    }
}

/// One human line per record (`None` for the purely technical ones).
fn describe(loop_id: &str, state: &LoopState, r: &LoadedRecord) -> Option<String> {
    let Some(ev) = r.event() else {
        return Some(
            style(format!("({} from a newer agentpit)", r.kind))
                .dim()
                .to_string(),
        );
    };
    Some(match ev {
        LoopEvent::LoopCreated(c) => format!(
            "created {} — {} ({})",
            style(&c.title).bold(),
            c.blueprint.name,
            c.blueprint.rev
        ),
        // Epoch 1 created the loop and 2 is its first runner: only later epochs are
        // restarts.
        LoopEvent::WriterOpened(w) if w.epoch > 2 => style(format!(
            "runner restarted (epoch {}){}",
            w.epoch,
            if w.truncated_tail_bytes > 0 {
                format!(", dropped a torn {}-byte tail", w.truncated_tail_bytes)
            } else {
                String::new()
            }
        ))
        .yellow()
        .to_string(),
        LoopEvent::WriterOpened(_) | LoopEvent::WriterClosed(_) | LoopEvent::StepSpawned(_) => {
            return None;
        }
        LoopEvent::LoopStarted => "started".into(),
        LoopEvent::LoopPaused(p) => style(format!("paused ({})", p.mode)).yellow().to_string(),
        LoopEvent::LoopResumed => "resumed".into(),
        LoopEvent::LoopStopRequested(s) => style(format!(
            "stopping ({}){}",
            s.mode,
            s.reason
                .as_deref()
                .map(|r| format!(": {r}"))
                .unwrap_or_default()
        ))
        .yellow()
        .to_string(),
        LoopEvent::LoopFinished(f) => {
            let text = format!(
                "finished {} ({}{}){}",
                f.status,
                f.reason,
                f.node
                    .as_deref()
                    .map(|n| format!(" at {n}"))
                    .unwrap_or_default(),
                f.detail
                    .as_deref()
                    .map(|d| format!(": {d}"))
                    .unwrap_or_default()
            );
            match f.status {
                FinishStatus::Succeeded => style(text).green().bold().to_string(),
                FinishStatus::Failed => style(text).red().bold().to_string(),
                _ => style(text).yellow().bold().to_string(),
            }
        }
        LoopEvent::BudgetChanged(b) => format!(
            "budget now {} steps, {}s compute, {} at once",
            b.budget.max_steps, b.budget.max_active_secs, b.budget.max_parallel
        ),
        LoopEvent::IterationStarted(i) => {
            let max = state
                .repeats
                .get(&instance_key(
                    &i.repeat,
                    &i.iter[..i.iter.len().saturating_sub(1)],
                ))
                .map(|r| r.max_iterations)
                .unwrap_or_default();
            format!(
                "{} {} {}/{max}{}",
                style("↻").cyan(),
                i.repeat,
                i.iter.last().copied().unwrap_or_default(),
                if i.feedback.is_empty() {
                    String::new()
                } else {
                    format!(" with {} feedback item(s)", i.feedback.len())
                }
            )
        }
        LoopEvent::IterationFinished(i) => format!(
            "  {} iteration {} → {}",
            i.repeat,
            i.iter.last().copied().unwrap_or_default(),
            i.decision
        ),
        LoopEvent::NodeSkipped(s) => style(format!(
            "· {} skipped ({})",
            instance_key(&s.node, &s.iter),
            s.reason
        ))
        .dim()
        .to_string(),
        LoopEvent::StepStarted(s) if s.kind == NodeKind::Repeat => return None,
        LoopEvent::StepStarted(s) => {
            let who = s
                .assignee
                .as_ref()
                .map(|a| match &a.role {
                    Some(role) => format!(" → {role}@{}", a.backend),
                    None => format!(" → {}", a.backend),
                })
                .or_else(|| {
                    s.command
                        .as_ref()
                        .map(|c| format!(" $ {}", clamp_text(c, 80)))
                })
                .unwrap_or_default();
            let cause = match s.cause {
                StartCause::Ready => String::new(),
                c => format!(" ({c})"),
            };
            format!("{} {}{who}{cause}", style("▶").cyan(), s.step_id)
        }
        LoopEvent::StepFinished(f) => {
            if state
                .step(&f.step_id)
                .is_some_and(|s| s.kind == NodeKind::Repeat)
            {
                return None;
            }
            let mark = match f.outcome {
                Outcome::Ok => style("✓").green(),
                Outcome::Cancelled => style("■").yellow(),
                _ => style("✗").red(),
            };
            let note = f
                .error
                .as_deref()
                .or(f.excerpt.as_deref())
                .or(f.verdict.as_ref().and_then(|v| v.findings.as_deref()))
                .map(|t| {
                    format!(
                        " — {}",
                        clamp_text(t.lines().next().unwrap_or_default(), 120)
                    )
                })
                .unwrap_or_default();
            format!(
                "{mark} {} {} in {}{note}",
                f.step_id,
                f.outcome,
                secs(f.elapsed_ms)
            )
        }
        LoopEvent::StepInterrupted(i) => style(format!(
            "! {} was interrupted when its runner stopped",
            i.step_id
        ))
        .yellow()
        .to_string(),
        LoopEvent::GateOpened(g) => {
            let options: Vec<&str> = g.options.iter().map(|o| o.id.as_str()).collect();
            format!(
                "{} {} [{}] {}\n       answer: agentpit loop gate {} {} <{}>",
                style("?").magenta().bold(),
                g.gate_id,
                g.kind,
                g.prompt,
                short(loop_id),
                g.gate_id,
                options.join("|")
            )
        }
        LoopEvent::GateResolved(g) => format!(
            "  {} answered {} by {}{}",
            g.gate_id,
            style(&g.option).bold(),
            g.by.client.as_deref().unwrap_or(g.by.kind.as_str()),
            g.comment
                .as_deref()
                .map(|c| format!(": {c}"))
                .unwrap_or_default()
        ),
        LoopEvent::GateCancelled(g) => format!("  {} closed ({})", g.gate_id, g.reason),
        LoopEvent::InstructionReceived(i) => format!(
            "  instruction {}{}: {}",
            i.instruction_id,
            i.target
                .as_deref()
                .map(|t| format!(" for {t}"))
                .unwrap_or_default(),
            clamp_text(&i.text, 120)
        ),
        LoopEvent::Warning(w) => style(format!("warning: {}", w.message))
            .yellow()
            .to_string(),
    })
}

// ---------------------------------------------------------------------------------------
// validate.

fn print_diagnostics(diags: &[Diagnostic]) {
    for d in diags {
        let sev = match d.severity {
            Severity::Error => style("error").red().bold(),
            _ => style("warning").yellow(),
        };
        let at = if d.path.is_empty() {
            String::new()
        } else {
            format!(" {}", style(&d.path).dim())
        };
        println!("{sev}[{}]{at}: {}", d.code, d.message);
    }
}

fn validate_file(file: &Path, json: bool) -> Result<()> {
    let text = std::fs::read_to_string(file).with_context(|| format!("read {}", file.display()))?;
    let doc: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("{} is not JSON", file.display()))?;
    let v = validate(&doc, &crate::loops::control::validate_env());
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "rev": v.rev,
                "runnable": v.is_runnable(),
                "diagnostics": v.diagnostics,
                "bounds": v.bounds,
            }))?
        );
    } else {
        print_diagnostics(&v.diagnostics);
        match (&v.blueprint, v.is_runnable()) {
            (Some(bp), true) => println!(
                "{} {} ({}){}",
                style("ok").green(),
                bp.name,
                v.rev,
                v.bounds
                    .map(|b| format!(", at most {} agent/check steps", b.worst_steps))
                    .unwrap_or_default()
            ),
            _ => println!("{} {}", style("not runnable").red(), file.display()),
        }
    }
    if !v.is_runnable() {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_read_as_one_line_each() {
        let rec = decode_line(
            r#"{"v":1,"seq":12,"ts":1,"kind":"step_finished","data":{"step_id":"test.i1.a1","outcome":"fail","elapsed_ms":14543,"exit_code":101,"error":"cargo test exited 101\nmore"}}"#,
        )
        .unwrap();
        let line = describe(
            "lp-0199a1b2c3d47e5f8a9b0c1d2e3f4a5b",
            &LoopState::default(),
            &rec,
        )
        .unwrap();
        let plain = console::strip_ansi_codes(&line);
        assert_eq!(plain, "✗ test.i1.a1 fail in 14s — cargo test exited 101");
        let gate = decode_line(
            r#"{"v":1,"seq":13,"ts":1,"kind":"gate_opened","data":{"gate_id":"g1","kind":"approval","prompt":"Ship?","options":[{"id":"approve"},{"id":"reject"}]}}"#,
        )
        .unwrap();
        let line = describe(
            "lp-0199a1b2c3d47e5f8a9b0c1d2e3f4a5b",
            &LoopState::default(),
            &gate,
        )
        .unwrap();
        assert!(
            console::strip_ansi_codes(&line)
                .contains("agentpit loop gate 2e3f4a5b g1 <approve|reject>"),
            "{line}"
        );
    }
}
