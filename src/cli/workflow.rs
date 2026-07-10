//! `agentpit workflow "<goal>"` — a model-driven multi-step workflow.
//!
//! A configured MANAGER backend (claude or codex) is launched as the orchestrator. It receives
//! the goal plus an injected orchestration system-prompt teaching it to drive the workflow by
//! shelling out to `agentpit` itself (`rescue`, `ensemble`), reading the results, and writing a
//! final synthesis. The manager's reasoning IS the orchestration logic — there is no static DAG.
//! Rust enforces the hard guardrails (recursion-depth cap via an inherited env var).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use std::collections::BTreeMap;

use super::{install_ctrlc_cancel, load_context, resolve_cwd, stdout_streamer};
use crate::auth::check_auth;
use crate::config::{HubConfig, RoleConfig};
use crate::dispatch::{Registries, dispatch};
use crate::events::{LegStatus, RunKind, RunLogger};
use crate::exec::{AskTier, McpConfigGuard, WorkflowManagerExec, is_supported_manager};
use crate::types::BackendId;
use crate::workflow::{guard, roles};

/// `agentpit workflow "<goal>"` — the CLI entry point. A thin wrapper over [`run_capture`] that
/// resolves the cwd, installs Ctrl-C cancellation, prints the leader line, streams the manager's
/// output to stdout, and discards the captured string (it is already on the terminal).
#[allow(clippy::too_many_arguments)]
pub async fn run(
    goal: String,
    manager: Option<BackendId>,
    agents: Option<Vec<BackendId>>,
    max_depth: Option<u32>,
    use_mcp: bool,
    cwd: Option<String>,
) -> Result<()> {
    // Resolve cwd and install Ctrl-C cancellation here (CLI-only concerns); `run_capture` itself
    // takes a ready cwd + cancel token so the MCP tool path can supply its own.
    let cwd = resolve_cwd(cwd)?;
    let cancel = CancellationToken::new();
    install_ctrlc_cancel(cancel.clone());

    // Stream every chunk straight to the terminal as it arrives.
    let terminal_sink = stdout_streamer();

    let output = run_capture(
        goal,
        manager,
        agents,
        max_depth,
        use_mcp,
        cwd,
        cancel,
        terminal_sink,
    )
    .await?;

    // The leader line is emitted inside `run_capture` THROUGH the terminal sink (so the MCP path's
    // no-op sink suppresses it and it never touches the captured string). Here we only need a
    // trailing newline if the manager's streamed output did not already end with one.
    if !output.ends_with('\n') {
        println!();
    }
    Ok(())
}

/// Launch the workflow manager and RETURN its synthesized output as a string instead of only
/// printing it. The single source of truth for starting a manager: [`run`] (CLI) and the
/// `run_workflow` MCP tool both call this.
///
/// `terminal_sink` receives each streamed chunk as it arrives (stdout for the CLI, a no-op for
/// MCP). The dashboard capture-file streamer needs the run id created by [`RunLogger::start`], so
/// the tee sink is built INSIDE this function — after the logger starts — and layered over
/// `terminal_sink`. The caller supplies `cwd` and `cancel`; the config is loaded here so both
/// callers stay simple.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_capture(
    goal: String,
    manager: Option<BackendId>,
    agents: Option<Vec<BackendId>>,
    max_depth: Option<u32>,
    use_mcp: bool,
    cwd: PathBuf,
    cancel: CancellationToken,
    terminal_sink: Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<String> {
    let ctx = load_context()?;
    let config = &ctx.loaded.config;

    // 1. Resolve the manager: explicit arg → [workflow.roles.manager] → [workflow].manager_backend
    //    → default backend. The manager role's persona applies whenever the role exists, even
    //    when the backend itself came from an earlier step in the chain.
    let (manager, manager_persona) = resolve_manager_backend(
        manager,
        &config.workflow.roles,
        config.workflow.manager_backend,
        config.default.backend,
    )?;
    if !is_supported_manager(manager) {
        anyhow::bail!("supported workflow managers: claude, codex (got: {manager})");
    }

    // 1b. MCP mode: the `--use-mcp` flag OR `[workflow].use_mcp`, but only the claude manager
    //     supports it (codex MCP mode is out of scope). Fall back to shell-out otherwise.
    let use_mcp = use_mcp || config.workflow.use_mcp;
    let mcp_mode = use_mcp && manager == BackendId::Claude;
    if use_mcp && !mcp_mode {
        eprintln!(
            "warning: --use-mcp is only supported for the claude manager; using shell-out mode for {manager}."
        );
    }

    // 2. Resolve the worker roster. With `[workflow.roles.*]` worker roles configured the
    //    roster is ROLES (casting is config-driven); otherwise the legacy flat backend list:
    //    explicit → config → all available minus the manager.
    let available: Vec<BackendId> = ctx.regs.available().into_iter().collect();
    let roster = build_role_roster(&config.workflow.roles, &available)?;
    if let Some(r) = &roster {
        for (name, reason) in &r.skipped {
            eprintln!("warning: workflow role '{name}' skipped: {reason}");
        }
        if agents.is_some() {
            eprintln!(
                "warning: --agents is ignored because [workflow.roles] is configured; roles win."
            );
        }
    }
    let agents = if roster.is_some() {
        Vec::new()
    } else {
        resolve_agents(agents, config, manager, &ctx.regs)
    };

    // 3. Resolve the depth ceiling: arg → config (default 3), clamped to a hard upper bound so a
    //    hostile `--max-depth` can never push the inherited depth toward `u32::MAX` and wrap.
    let max_depth = guard::clamp_max_depth(max_depth.unwrap_or(config.workflow.max_depth));
    let max_calls = config.workflow.max_calls_per_manager;

    // 4. Guard: bail if we are already at the ceiling (a manager re-invoking `agentpit workflow`).
    let depth = guard::check_not_exceeded(max_depth)?;

    // 6. Auth preflight the manager leg. No auto-login in the MVP — just a clear hint and bail.
    let auth = check_auth(manager).await;
    if !auth.ok {
        anyhow::bail!(
            "[{manager}] not authenticated. Run `{}`, or call `agentpit login {manager}`.",
            auth.login_command
        );
    }

    // 7. Path to re-invoke for the dispatch grammar in the prompt.
    let self_path = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "agentpit".into());

    // 8. Start the run first: the prompt (step 9) and the child_env (step 10) both need the
    //    run id, which only exists once the logger has started.
    let logger = RunLogger::start(RunKind::Workflow, &[manager], &cwd);
    logger.member_started(manager, false);

    // 9. Build the manager prompt (needs the run id from step 8 as the parent run id). MCP mode
    //    gets a variant that drives the workflow via MCP tools instead of the Bash grammar.
    //    The human back-channel + its question-discipline block are injected only when enabled
    //    (default off until dogfooded). The manager is always full-autonomy today → High tier.
    let ask = config.workflow.enable_ask_human.then_some(AskTier::High);
    let prompt_roles = PromptRoles {
        roster: roster.as_ref().map(|r| r.lines.as_str()),
        persona: manager_persona.as_deref(),
    };
    let prompt = if mcp_mode {
        build_manager_prompt_mcp(
            &goal,
            &agents,
            depth,
            max_depth,
            max_calls,
            logger.run_id(),
            ask,
            &prompt_roles,
        )
    } else {
        build_manager_prompt(
            &goal,
            &agents,
            &self_path,
            depth,
            max_depth,
            max_calls,
            logger.run_id(),
            ask,
            &prompt_roles,
        )
    };

    // 10. In MCP mode, write the temp MCP server config; the guard removes it on drop and is
    //     held until after the dispatch so claude can read it for the whole run.
    let mcp_guard = if mcp_mode {
        Some(McpConfigGuard::write(&self_path, logger.run_id())?)
    } else {
        None
    };

    // 11. Build a one-off Registries holding only the manager adapter and dispatch the leg.
    let mut child_env = guard::child_env(depth.saturating_add(1), logger.run_id(), &self_path);
    if config.workflow.enable_ask_human {
        // Authorize ONLY the manager leg to reach the human via `agentpit ask`. `exec::base`
        // strips this token from every backend spawn, so a worker the manager dispatches cannot
        // pass the gate (which requires the token to equal AGENTPIT_PARENT_RUN_ID). The MCP path
        // ignores the token entirely — workers there have no MCP channel, so they are already
        // structurally isolated.
        child_env.push((
            crate::ask::ENV_ASK_ALLOWED.to_string(),
            logger.run_id().to_string(),
        ));
    }
    let manager_exec = WorkflowManagerExec {
        backend: manager,
        child_env,
        mcp_config_path: mcp_guard.as_ref().map(|g| g.path().to_path_buf()),
    };
    let mut regs = Registries::empty();
    regs.execs.insert(manager, Box::new(manager_exec));

    // The leader line goes through `terminal_sink`, NOT `println!`: the CLI sink (stdout_streamer)
    // shows it on the terminal, while the MCP tool's no-op sink suppresses it. A raw `println!`
    // here would write to process stdout unconditionally and corrupt the JSON-RPC framing when
    // `run_workflow` runs inside `agentpit mcp serve` (see `mcp::server::run_stdio`). It is also
    // kept out of the returned capture string so the MCP tool's synthesis stays clean.
    let cast = match &roster {
        Some(r) => format!("roles={}", r.names.join(", ")),
        None => format!("agents={}", agents_csv(&agents)),
    };
    terminal_sink(&format!(
        "[workflow manager={manager} depth={depth}/{max_depth} mcp={mcp_mode} {cast}]\n"
    ));

    // Tee streamed output to the caller's terminal sink AND the dashboard's capture file (mirror
    // rescue). The file streamer needs `logger.run_id()`, so it is built here — after the logger
    // started — and layered over the caller-supplied `terminal_sink`.
    let to_file = crate::events::output_streamer(logger.run_id(), manager, false);
    let on_chunk: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(move |c: &str| {
        terminal_sink(c);
        to_file(c);
    });

    let started = Instant::now();
    let result = dispatch(manager, &prompt, &cwd, cancel, on_chunk, &regs).await;

    // 12. Record the outcome. `mcp_guard` is still in scope here, so the temp MCP config stays
    //     on disk for the whole dispatch and is removed when this function returns.
    match result {
        Ok(res) => {
            if res.auth_failed {
                logger.member_finished(
                    manager,
                    false,
                    LegStatus::Error,
                    started.elapsed().as_millis() as u64,
                    None,
                    Some("auth failure during execution".into()),
                );
                logger.finished(LegStatus::Error);
                anyhow::bail!(
                    "[{manager}] auth failure during execution. Run `{}`, or call `agentpit login {manager}`.",
                    auth.login_command
                );
            }
            logger.member_finished(
                manager,
                false,
                LegStatus::Ok,
                started.elapsed().as_millis() as u64,
                Some(res.output.len()),
                None,
            );
            logger.finished(LegStatus::Ok);
            Ok(res.output)
        }
        Err(err) => {
            let msg = format!("{err:#}");
            logger.member_finished(
                manager,
                false,
                LegStatus::Error,
                started.elapsed().as_millis() as u64,
                None,
                Some(msg.clone()),
            );
            logger.finished(LegStatus::Error);
            Err(err)
        }
    }
}

/// Resolve the worker roster the manager may dispatch to: explicit `--agents` → configured
/// `default_agents` → every available backend minus the manager itself.
fn resolve_agents(
    explicit: Option<Vec<BackendId>>,
    config: &HubConfig,
    manager: BackendId,
    regs: &Registries,
) -> Vec<BackendId> {
    if let Some(a) = explicit {
        return a;
    }
    if !config.workflow.default_agents.is_empty() {
        return config.workflow.default_agents.clone();
    }
    let mut available: Vec<BackendId> = regs
        .available()
        .into_iter()
        .filter(|b| *b != manager)
        .collect();
    available.sort();
    available
}

fn agents_csv(agents: &[BackendId]) -> String {
    agents
        .iter()
        .map(BackendId::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Resolve the manager backend and its optional persona. Resolution order for the backend:
/// explicit CLI arg → `[workflow.roles.manager]` (first claude|codex in its list) →
/// `[workflow].manager_backend` → the default backend. The persona applies whenever the manager
/// role exists — a persona-only role (empty `backends`) still colors a legacy-resolved manager.
/// Pure and unit-tested; the `is_supported_manager` check on the final choice stays in
/// `run_capture` (an explicit arg may still name an unsupported backend).
fn resolve_manager_backend(
    explicit: Option<BackendId>,
    role_map: &BTreeMap<String, RoleConfig>,
    manager_backend: Option<BackendId>,
    default_backend: BackendId,
) -> Result<(BackendId, Option<String>)> {
    let manager_role = roles::resolve_manager(role_map, is_supported_manager)?;
    let backend = explicit
        .or(manager_role.as_ref().and_then(|m| m.backend))
        .or(manager_backend)
        .unwrap_or(default_backend);
    Ok((backend, manager_role.and_then(|m| m.prompt)))
}

/// The rendered role roster for a run: the manager-facing block plus the resolved role names
/// for the leader line, and the roles that could not be resolved (warned, then skipped).
#[derive(Debug, Clone, PartialEq)]
struct RoleRoster {
    /// Pre-rendered roster lines: `  <name> (<backend>): <persona summary>` joined by newline.
    lines: String,
    names: Vec<String>,
    skipped: Vec<(String, String)>,
}

/// Build the role roster when worker roles are configured. `Ok(None)` = no worker roles (legacy
/// flat-backend roster applies). Roles whose backends are all unavailable are collected into
/// `skipped` rather than aborting the run — but if EVERY worker role fails to resolve the
/// workflow cannot dispatch anywhere, which is a hard error.
fn build_role_roster(
    role_map: &BTreeMap<String, RoleConfig>,
    available: &[BackendId],
) -> Result<Option<RoleRoster>> {
    if roles::worker_roles(role_map).next().is_none() {
        return Ok(None);
    }
    let mut lines = Vec::new();
    let mut names = Vec::new();
    let mut skipped = Vec::new();
    for (name, role) in roles::worker_roles(role_map) {
        match roles::resolve_role(name, role_map, available) {
            Ok(resolved) => {
                lines.push(format!(
                    "  {name} ({backend}): {summary}",
                    backend = resolved.backend,
                    summary = roles::summary_line(role)
                ));
                names.push(name.clone());
            }
            Err(err) => skipped.push((name.clone(), format!("{err:#}"))),
        }
    }
    if names.is_empty() {
        let detail = skipped
            .iter()
            .map(|(n, e)| format!("{n}: {e}"))
            .collect::<Vec<_>>()
            .join("; ");
        anyhow::bail!("no configured worker role could be resolved ({detail})");
    }
    Ok(Some(RoleRoster {
        lines: lines.join("\n"),
        names,
        skipped,
    }))
}

/// Role-mode additions to the manager prompt. The `Default` (both `None`) keeps the legacy
/// flat-backend prompt BYTE-IDENTICAL — pinned by the `legacy_prompt_is_byte_identical_*`
/// regression tests below, so prompt drift for existing users is a test failure, not a surprise.
#[derive(Debug, Default, Clone)]
pub struct PromptRoles<'a> {
    /// Pre-rendered roster lines (`  <name> (<backend>): <summary>` joined by newline).
    pub roster: Option<&'a str>,
    /// Manager persona from `[workflow.roles.manager]`.
    pub persona: Option<&'a str>,
}

/// The manager-persona block injected right below the orchestrator header, or empty.
fn persona_block(persona: Option<&str>) -> String {
    match persona {
        None => String::new(),
        Some(p) => format!(
            "MANAGER PERSONA (from [workflow.roles.manager]):\n{p}\n\n",
            p = p.trim()
        ),
    }
}

/// The manager-facing "question discipline" block, gated on the [`AskTier`]. Injected into the
/// manager prompt ONLY when `[workflow].enable_ask_human` is on. `ask_invocation` is the prose
/// name of the back-channel in this mode (e.g. "the `agentpit ask` command"); `ask_grammar` is
/// a concrete one-line example of calling it. Returns a block ending in a blank line so it slots
/// cleanly between the BUDGET and PROCEDURE sections.
fn question_discipline_block(ask_tier: AskTier, ask_invocation: &str, ask_grammar: &str) -> String {
    let when_to_ask = match ask_tier {
        AskTier::High => {
            "\
  - DESTRUCTIVE or IRREVERSIBLE actions a human cannot cheaply undo: deleting or\n\
    force-overwriting non-generated files, `git push --force` / history rewrites,\n\
    dropping data, deploying / releasing, mutating anything outside this repo, or\n\
    spending money / touching production. If unsure whether it is reversible, ask.\n\
  - Nothing else. Resolve ambiguous requirements and A/B forks yourself with the most\n\
    standard, reversible choice and a one-line 'ASSUMED: <choice> (<reason>)' note."
        }
        AskTier::Medium => {
            "\
  - DESTRUCTIVE or IRREVERSIBLE actions a human cannot cheaply undo (as above).\n\
  - A genuinely AMBIGUOUS requirement whose branches diverge materially and that you\n\
    cannot resolve by re-reading the goal. Otherwise decide and note an 'ASSUMED:' line."
        }
        AskTier::Low => {
            "\
  - DESTRUCTIVE or IRREVERSIBLE actions a human cannot cheaply undo.\n\
  - A genuinely ambiguous requirement with materially diverging branches.\n\
  - A genuine A/B fork with no clearly safer or more standard default."
        }
    };
    format!(
        "HUMAN BACK-CHANNEL — QUESTION DISCIPLINE (tier: {tier}):\n\
You are the SOLE window between this workflow and the human, who supervises YOU, not the\n\
workers. Reach the human with {ask_invocation} ONLY when a rule below fires — an unnecessary\n\
ask wastes the scarcest resource here, the human's attention.\n\
\n\
ASK ONLY WHEN:\n\
{when_to_ask}\n\
\n\
HOW TO ASK: pre-frame a CLOSED question with explicit options; mark kind 'blocking' (a worker\n\
  is stalled) or 'review' (nothing blocked); at most two sentences of context; never paste\n\
  worker output. One ask per decision; never ask the same thing twice.\n\
  INVOKE:  {ask_grammar}\n\
\n\
WORKERS ARE NEVER THE HUMAN: only YOU may ask. Workers have no back-channel by design; if a\n\
  worker says it 'needs the user', that is a signal for YOU to decide or, if a rule fires, to\n\
  ask on its behalf with pre-framed options.\n\
\n\
IF UNAVAILABLE: {ask_invocation} may return the literal HUMAN_UNAVAILABLE (no answer in time).\n\
  This is NOT an error and MUST NOT stop the workflow — choose the safe, most reversible\n\
  branch, record 'HUMAN_UNAVAILABLE — proceeded with <branch> (<reason>)', and surface it in\n\
  your SYNTHESIS.\n\
\n",
        tier = ask_tier.as_str(),
    )
}

/// The manager-facing "conversation layer" block (design ①④): how to RECORD a handoff/board note
/// onto the run transcript, and how to run ONE refutation pass over a stuck sub-task before
/// discarding it. Gated on the same `[workflow].enable_ask_human` switch as the question-discipline
/// block, so the default manager prompt is byte-identical. `note_grammar` / `refute_grammar` are
/// concrete one-line invocations for this mode (shell-out vs MCP). Returns a block ending in a
/// blank line so it slots between the question-discipline block and PROCEDURE.
fn conversation_discipline_block(note_grammar: &str, refute_grammar: &str) -> String {
    format!(
        "CONVERSATION LAYER — HANDOFFS & REFUTATION:\n\
You drive the workflow; workers are one-shot and share no memory. Two durable moves are yours:\n\
\n\
  HANDOFF: when one worker's result must seed the next sub-task, RECORD it on the run transcript\n\
    before the next dispatch, then carry the relevant facts into that dispatch's prompt yourself.\n\
    Use kind 'handoff' and set --from to the worker that produced it; use kind 'board' for a fact\n\
    several later sub-tasks will reuse. Recording is fire-and-forget — it does not block.\n\
    INVOKE:  {note_grammar}\n\
\n\
  STUCK SUB-TASK: before you DISCARD a worker's output as a dead end, run ONE refutation pass —\n\
    an adversarial critic challenges the candidate, then a defender rebuts or fixes it — and then\n\
    YOU adjudicate: ADOPT the revised candidate, KEEP the original, or DISCARD and re-plan. One\n\
    pass, never a loop; it is advisory, so a failed leg does not block you.\n\
    INVOKE:  {refute_grammar}\n\
\n",
    )
}

/// Build the orchestration system-prompt prepended to the user goal.
///
/// Pure and unit-tested: it threads the worker roster, the goal, the depth/budget lines, the
/// self path, the dispatch grammar, the optional human-back-channel discipline block, and the
/// instruction to END with a synthesis section. `ask = Some(tier)` injects the discipline block
/// (and teaches the `agentpit ask` grammar); `None` omits it entirely. `roles` switches the
/// roster + dispatch grammar to role mode and/or injects the manager persona; the default
/// (both `None`) is byte-identical to the pre-roles prompt.
#[allow(clippy::too_many_arguments)]
pub fn build_manager_prompt(
    goal: &str,
    agents: &[BackendId],
    self_path: &str,
    depth: u32,
    max_depth: u32,
    max_calls: u32,
    parent_run_id: &str,
    ask: Option<AskTier>,
    roles: &PromptRoles<'_>,
) -> String {
    let agents_csv = agents_csv(agents);
    // The model embeds `self_path` directly into Bash commands; single-quote it so a binary path
    // containing spaces or shell metacharacters (e.g. "/Users/a b/bin/agentpit") still parses as
    // one argument. POSIX single-quote escaping: close, emit an escaped quote, reopen.
    let self_path = format!("'{}'", self_path.replace('\'', "'\\''"));
    let self_path = self_path.as_str();
    // The shell-out manager reaches the human by running `<self_path> ask "<q>" [--option ...]`.
    let discipline = match ask {
        Some(tier) => question_discipline_block(
            tier,
            "the `agentpit ask` command",
            &format!(
                "{self_path} ask \"<question>\" [--option A --option B] [--kind blocking|review]"
            ),
        ),
        None => String::new(),
    };
    let discipline = discipline.as_str();
    // The conversation layer (handoffs + refutation) rides the same enable_ask_human switch, so
    // the default prompt is unchanged. Shell-out grammar: `<self_path> note …` / `… refute …`.
    let conversation = if ask.is_some() {
        conversation_discipline_block(
            &format!("{self_path} note \"<context>\" --kind handoff [--from <worker>]"),
            &format!(
                "{self_path} refute \"<candidate>\" --task \"<sub-task>\" [--critic <id>] [--defender <id>]"
            ),
        )
    } else {
        String::new()
    };
    let conversation = conversation.as_str();
    // Roster + dispatch-grammar blocks: role mode teaches `rescue --role`, legacy mode keeps the
    // flat backend grammar. Composed as blocks so the legacy assembly stays byte-identical.
    let (roster_block, grammar_block) = match roles.roster {
        Some(lines) => (
            format!(
                "AVAILABLE ROLES (dispatch to a role; it resolves to the backend in parentheses):\n\
                 {lines}\n\
                 \x20 Dispatch only to a ROLE above; do NOT invent role names; do NOT dispatch to yourself.\n"
            ),
            format!(
                "DISPATCH GRAMMAR (use your Bash tool):\n\
                 \x20 One role:      {self_path} rescue --role <name> \"<sub-task>\"\n\
                 \x20 Parallel fan:  {self_path} ensemble <id> <id> ... \"<prompt>\" [--aggregator <id>]\n\
                 \x20 --role is REQUIRED for rescue; the backends in parentheses are informational \
                 (ensemble still takes backend ids). Quote sub-tasks. For multi-line, use a bash heredoc.\n"
            ),
        ),
        // NB: the legacy branch reproduces the PRE-ROLES prompt byte-for-byte, including the
        // flush-left continuation lines (`\<newline>` strips leading whitespace) — pinned by
        // `legacy_prompt_is_byte_identical_without_roles`.
        None => (
            format!(
                "AVAILABLE WORKER BACKENDS: {agents_csv}\n\
                 Pick the best fit per sub-task. Dispatch only to a worker above; do NOT dispatch to yourself.\n"
            ),
            format!(
                "DISPATCH GRAMMAR (use your Bash tool):\n\
                 One backend:   {self_path} rescue --backend <id> \"<sub-task>\"\n\
                 Parallel fan:  {self_path} ensemble <id> <id> ... \"<prompt>\" [--aggregator <id>]\n\
                 --backend is REQUIRED for rescue. Quote sub-tasks. For multi-line, use a bash heredoc.\n"
            ),
        ),
    };
    let persona = persona_block(roles.persona);
    format!(
        "=== AGENTPIT WORKFLOW ORCHESTRATOR ===\n\
You are the MANAGER agent for a multi-step coding workflow. Decompose the goal into\n\
sub-tasks, dispatch each to the best worker backend, read results, and write a final\n\
synthesis. Your LAST message MUST be the synthesis — do not stop after the last dispatch.\n\
\n\
{persona}\
{roster_block}\
\n\
{grammar_block}\
\n\
BUDGET: workflow depth {depth}/{max_depth}; aim for <= {max_calls} sub-dispatch calls.\n\
  The system REJECTS any nested workflow past the depth ceiling. Plan within budget.\n\
\n\
{discipline}\
{conversation}\
PROCEDURE:\n\
  1. Briefly state your plan (the sub-tasks).\n\
  2. Dispatch each sub-task; read its output; adjust the remaining plan as needed.\n\
  3. If a worker exits non-zero, note it inline and continue — do not abort the whole run.\n\
  4. End with a clearly-labelled SYNTHESIS section integrating all results.\n\
\n\
PARENT RUN ID: {parent_run_id}   (correlation only; do not modify)\n\
\n\
=== USER GOAL ===\n\
{goal}\n"
    )
}

/// MCP-mode variant of [`build_manager_prompt`].
///
/// Instructs the manager to orchestrate via the `mcp__agentpit__*` MCP tools rather than shelling
/// out to the `agentpit` binary, so it carries no Bash dispatch grammar and needs no self path.
#[allow(clippy::too_many_arguments)]
pub fn build_manager_prompt_mcp(
    goal: &str,
    agents: &[BackendId],
    depth: u32,
    max_depth: u32,
    max_calls: u32,
    parent_run_id: &str,
    ask: Option<AskTier>,
    roles: &PromptRoles<'_>,
) -> String {
    let agents_csv = agents_csv(agents);
    // The MCP manager reaches the human via the ask_human tool rather than a shell command.
    let discipline = match ask {
        Some(tier) => question_discipline_block(
            tier,
            "the mcp__agentpit__ask_human tool",
            "mcp__agentpit__ask_human  {\"prompt\":\"<question>\",\"options\":[\"A\",\"B\"],\"kind\":\"blocking|review\"}",
        ),
        None => String::new(),
    };
    let discipline = discipline.as_str();
    // The conversation layer rides the same switch as the question-discipline block. MCP grammar:
    // the post_note / refute tools rather than shell commands.
    let conversation = if ask.is_some() {
        conversation_discipline_block(
            "mcp__agentpit__post_note  {\"body\":\"<context>\",\"kind\":\"handoff\",\"from\":\"<worker>\"}",
            "mcp__agentpit__refute  {\"candidate\":\"<candidate>\",\"task\":\"<sub-task>\"}",
        )
    } else {
        String::new()
    };
    let conversation = conversation.as_str();
    // Roster + tools blocks: role mode teaches `dispatch_task {{"role": ...}}`, legacy mode the
    // flat backend argument. Composed as blocks so the legacy assembly stays byte-identical.
    let (roster_block, tools_block) = match roles.roster {
        Some(lines) => (
            format!(
                "AVAILABLE ROLES (dispatch to a role; it resolves to the backend in parentheses):\n\
                 {lines}\n\
                 \x20 Dispatch only to a ROLE above; do NOT invent role names; do NOT dispatch to yourself.\n"
            ),
            "ORCHESTRATION TOOLS (use these MCP tools; do NOT shell out to agentpit):\n\
             \x20 mcp__agentpit__list_backends  — list available backends + their auth/transport state.\n\
             \x20 mcp__agentpit__dispatch_task  — run ONE role. Args: {\"role\":\"<name>\",\"task\":\"<sub-task>\"} \
             ({\"backend\":\"<id>\"} is the legacy alternative; never pass both).\n\
             \x20 mcp__agentpit__run_ensemble   — fan out in parallel. Args: {\"members\":[\"<id>\",...],\"prompt\":\"<prompt>\",\"aggregator\":\"<id>\"} (aggregator optional).\n"
                .to_string(),
        ),
        // NB: the legacy branch reproduces the PRE-ROLES prompt byte-for-byte, including the
        // flush-left continuation lines (`\<newline>` strips leading whitespace) — pinned by
        // `legacy_mcp_prompt_is_byte_identical_without_roles`.
        None => (
            format!(
                "AVAILABLE WORKER BACKENDS: {agents_csv}\n\
                 Pick the best fit per sub-task. Dispatch only to a worker above; do NOT dispatch to yourself.\n"
            ),
            "ORCHESTRATION TOOLS (use these MCP tools; do NOT shell out to agentpit):\n\
             mcp__agentpit__list_backends  — list available backends + their auth/transport state.\n\
             mcp__agentpit__dispatch_task  — run ONE backend. Args: {\"backend\":\"<id>\",\"task\":\"<sub-task>\"}.\n\
             mcp__agentpit__run_ensemble   — fan out in parallel. Args: {\"members\":[\"<id>\",...],\"prompt\":\"<prompt>\",\"aggregator\":\"<id>\"} (aggregator optional).\n"
                .to_string(),
        ),
    };
    let persona = persona_block(roles.persona);
    format!(
        "=== AGENTPIT WORKFLOW ORCHESTRATOR (MCP MODE) ===\n\
You are the MANAGER agent for a multi-step coding workflow. Decompose the goal into\n\
sub-tasks, dispatch each to the best worker backend, read results, and write a final\n\
synthesis. Your LAST message MUST be the synthesis — do not stop after the last dispatch.\n\
\n\
{persona}\
{roster_block}\
\n\
{tools_block}\
\n\
BUDGET: workflow depth {depth}/{max_depth}; aim for <= {max_calls} sub-dispatch calls.\n\
  The system REJECTS any nested workflow past the depth ceiling. Plan within budget.\n\
\n\
{discipline}\
{conversation}\
PROCEDURE:\n\
  1. Briefly state your plan (the sub-tasks).\n\
  2. Call the MCP tools above; read each result; adjust the remaining plan as needed.\n\
  3. If a worker fails, note it inline and continue — do not abort the whole run.\n\
  4. End with a clearly-labelled SYNTHESIS section integrating all results.\n\
\n\
PARENT RUN ID: {parent_run_id}   (correlation only; do not modify)\n\
\n\
=== USER GOAL ===\n\
{goal}\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_includes_agents_goal_grammar_and_synthesis() {
        let agents = [BackendId::Gemini, BackendId::Opencode, BackendId::Codex];
        let text = build_manager_prompt(
            "ship the feature",
            &agents,
            "/usr/local/bin/agentpit",
            1,
            3,
            8,
            "run-42",
            None, &PromptRoles::default());
        // Each agent id appears.
        assert!(text.contains("gemini"));
        assert!(text.contains("opencode"));
        assert!(text.contains("codex"));
        // The goal text.
        assert!(text.contains("ship the feature"));
        // The dispatch grammar.
        assert!(text.contains("agentpit rescue --backend") || text.contains("rescue --backend"));
        assert!(text.contains("ensemble"));
        // The self path.
        assert!(text.contains("/usr/local/bin/agentpit"));
        // Depth/budget lines.
        assert!(text.contains("depth 1/3"));
        assert!(text.contains("run-42"));
        // Ends with a synthesis instruction.
        assert!(text.contains("SYNTHESIS"));
    }

    #[test]
    fn self_path_is_shell_quoted_for_paths_with_spaces() {
        let agents = [BackendId::Gemini];
        let text = build_manager_prompt(
            "goal",
            &agents,
            "/Users/a b/bin/agentpit",
            1,
            3,
            8,
            "run-1",
            None, &PromptRoles::default());
        // The path is single-quoted so it survives word-splitting in the model's Bash commands.
        assert!(text.contains("'/Users/a b/bin/agentpit' rescue --backend"));
        assert!(text.contains("'/Users/a b/bin/agentpit' ensemble"));
    }

    #[test]
    fn mcp_prompt_uses_mcp_tools_and_omits_bash_grammar() {
        let agents = [BackendId::Gemini, BackendId::Codex];
        let text = build_manager_prompt_mcp("ship the feature", &agents, 1, 3, 8, "run-9", None, &PromptRoles::default());
        // The MCP tool names are present.
        assert!(text.contains("mcp__agentpit__list_backends"));
        assert!(text.contains("mcp__agentpit__dispatch_task"));
        assert!(text.contains("mcp__agentpit__run_ensemble"));
        // The agents, goal, budget and synthesis instruction carry over.
        assert!(text.contains("gemini"));
        assert!(text.contains("codex"));
        assert!(text.contains("ship the feature"));
        assert!(text.contains("depth 1/3"));
        assert!(text.contains("run-9"));
        assert!(text.contains("SYNTHESIS"));
        // The Bash dispatch grammar is gone.
        assert!(!text.contains("rescue --backend"));
        assert!(!text.contains("your Bash tool"));
        assert!(!text.contains("agentpit ensemble"));
    }

    #[test]
    fn cli_prompt_has_no_mcp_tool_names() {
        // The CLI variant must stay free of the MCP tool surface.
        let agents = [BackendId::Gemini];
        let text = build_manager_prompt("goal", &agents, "/bin/agentpit", 1, 3, 8, "run-1", None, &PromptRoles::default());
        assert!(!text.contains("mcp__agentpit__"));
    }

    #[test]
    fn discipline_block_absent_when_ask_disabled() {
        let agents = [BackendId::Gemini];
        let cli = build_manager_prompt("goal", &agents, "/bin/agentpit", 1, 3, 8, "run-1", None, &PromptRoles::default());
        let mcp = build_manager_prompt_mcp("goal", &agents, 1, 3, 8, "run-1", None, &PromptRoles::default());
        for text in [&cli, &mcp] {
            assert!(
                !text.contains("HUMAN BACK-CHANNEL"),
                "discipline must be omitted when off"
            );
            assert!(!text.contains("HUMAN_UNAVAILABLE"));
            // The conversation layer (handoffs + refutation) is gated on the same switch.
            assert!(
                !text.contains("CONVERSATION LAYER"),
                "conversation block must be omitted when off"
            );
            assert!(!text.contains("refute"));
            // The base prompt is intact and still ends with the user goal + synthesis instruction.
            assert!(text.contains("PROCEDURE:"));
            assert!(text.contains("SYNTHESIS"));
            assert!(text.trim_end().ends_with("goal"));
        }
    }

    #[test]
    fn conversation_block_teaches_handoff_and_refute_when_enabled() {
        let agents = [BackendId::Gemini];
        let cli = build_manager_prompt(
            "goal",
            &agents,
            "/bin/agentpit",
            1,
            3,
            8,
            "run-1",
            Some(AskTier::High), &PromptRoles::default());
        assert!(cli.contains("CONVERSATION LAYER"));
        // The CLI grammar for both moves is taught.
        assert!(cli.contains("note \"<context>\" --kind handoff"));
        assert!(cli.contains("refute \"<candidate>\" --task"));
        // The adjudication framing — ADOPT / KEEP / DISCARD, one pass.
        assert!(cli.contains("ADOPT the revised candidate"));
        assert!(cli.contains("never a loop"));
        // The block still slots before PROCEDURE, which ends the body (regression guard).
        assert!(cli.contains("PROCEDURE:"));
        assert!(cli.trim_end().ends_with("goal"));
    }

    #[test]
    fn mcp_conversation_block_points_at_post_note_and_refute_tools() {
        let agents = [BackendId::Gemini];
        let mcp = build_manager_prompt_mcp("goal", &agents, 1, 3, 8, "run-1", Some(AskTier::High), &PromptRoles::default());
        assert!(mcp.contains("CONVERSATION LAYER"));
        assert!(mcp.contains("mcp__agentpit__post_note"));
        assert!(mcp.contains("mcp__agentpit__refute"));
        // No shell-out note/refute grammar in MCP mode.
        assert!(!mcp.contains("note \"<context>\""));
    }

    #[test]
    fn high_tier_discipline_asks_only_on_destructive_not_ab_forks() {
        let agents = [BackendId::Gemini];
        let text = build_manager_prompt(
            "goal",
            &agents,
            "/bin/agentpit",
            1,
            3,
            8,
            "run-1",
            Some(AskTier::High), &PromptRoles::default());
        assert!(text.contains("HUMAN BACK-CHANNEL"));
        assert!(text.contains("tier: high"));
        assert!(text.contains("DESTRUCTIVE or IRREVERSIBLE"));
        assert!(text.contains("HUMAN_UNAVAILABLE"));
        assert!(text.contains("Workers have no back-channel"));
        // The CLI grammar is taught.
        assert!(text.contains("ask \"<question>\""));
        // High tier tells the manager to RESOLVE A/B forks itself, never adding the Low-tier
        // "ask on a genuine A/B fork" bullet.
        assert!(!text.contains("A genuine A/B fork"));
        // The synthesis instruction still comes last (regression guard).
        assert!(text.contains("PROCEDURE:"));
        assert!(text.trim_end().ends_with("goal"));
    }

    #[test]
    fn low_tier_discipline_invites_ab_fork_asks() {
        let agents = [BackendId::Gemini];
        let text = build_manager_prompt(
            "goal",
            &agents,
            "/bin/agentpit",
            1,
            3,
            8,
            "run-1",
            Some(AskTier::Low), &PromptRoles::default());
        assert!(text.contains("tier: low"));
        assert!(text.contains("A genuine A/B fork"));
    }

    #[test]
    fn mcp_discipline_points_at_ask_human_tool() {
        let agents = [BackendId::Gemini];
        let text = build_manager_prompt_mcp("goal", &agents, 1, 3, 8, "run-1", Some(AskTier::High), &PromptRoles::default());
        assert!(text.contains("HUMAN BACK-CHANNEL"));
        assert!(text.contains("mcp__agentpit__ask_human"));
        // No shell-out ask grammar in MCP mode.
        assert!(!text.contains("agentpit ask \"<question>\""));
    }

    // ---- roles: byte-identical legacy regression pins ----

    /// GOLDEN: with no roles configured the shell-mode prompt must stay EXACTLY what it was
    /// before the roles layer landed. If this test fails, existing users' manager prompts
    /// drifted — treat as a bug, not a test to update casually.
    #[test]
    fn legacy_prompt_is_byte_identical_without_roles() {
        let agents = [BackendId::Gemini];
        let text = build_manager_prompt("goal", &agents, "/bin/agentpit", 1, 3, 8, "run-1", None, &PromptRoles::default());
        let expected = "=== AGENTPIT WORKFLOW ORCHESTRATOR ===\n\
You are the MANAGER agent for a multi-step coding workflow. Decompose the goal into\n\
sub-tasks, dispatch each to the best worker backend, read results, and write a final\n\
synthesis. Your LAST message MUST be the synthesis — do not stop after the last dispatch.\n\
\n\
AVAILABLE WORKER BACKENDS: gemini\n\
Pick the best fit per sub-task. Dispatch only to a worker above; do NOT dispatch to yourself.\n\
\n\
DISPATCH GRAMMAR (use your Bash tool):\n\
One backend:   '/bin/agentpit' rescue --backend <id> \"<sub-task>\"\n\
Parallel fan:  '/bin/agentpit' ensemble <id> <id> ... \"<prompt>\" [--aggregator <id>]\n\
--backend is REQUIRED for rescue. Quote sub-tasks. For multi-line, use a bash heredoc.\n\
\n\
BUDGET: workflow depth 1/3; aim for <= 8 sub-dispatch calls.\n\
The system REJECTS any nested workflow past the depth ceiling. Plan within budget.\n\
\n\
PROCEDURE:\n\
1. Briefly state your plan (the sub-tasks).\n\
2. Dispatch each sub-task; read its output; adjust the remaining plan as needed.\n\
3. If a worker exits non-zero, note it inline and continue — do not abort the whole run.\n\
4. End with a clearly-labelled SYNTHESIS section integrating all results.\n\
\n\
PARENT RUN ID: run-1   (correlation only; do not modify)\n\
\n\
=== USER GOAL ===\n\
goal\n";
        assert_eq!(text, expected);
    }

    /// GOLDEN: the MCP-mode twin of the byte-identical pin above.
    #[test]
    fn legacy_mcp_prompt_is_byte_identical_without_roles() {
        let agents = [BackendId::Gemini];
        let text = build_manager_prompt_mcp("goal", &agents, 1, 3, 8, "run-1", None, &PromptRoles::default());
        let expected = "=== AGENTPIT WORKFLOW ORCHESTRATOR (MCP MODE) ===\n\
You are the MANAGER agent for a multi-step coding workflow. Decompose the goal into\n\
sub-tasks, dispatch each to the best worker backend, read results, and write a final\n\
synthesis. Your LAST message MUST be the synthesis — do not stop after the last dispatch.\n\
\n\
AVAILABLE WORKER BACKENDS: gemini\n\
Pick the best fit per sub-task. Dispatch only to a worker above; do NOT dispatch to yourself.\n\
\n\
ORCHESTRATION TOOLS (use these MCP tools; do NOT shell out to agentpit):\n\
mcp__agentpit__list_backends  — list available backends + their auth/transport state.\n\
mcp__agentpit__dispatch_task  — run ONE backend. Args: {\"backend\":\"<id>\",\"task\":\"<sub-task>\"}.\n\
mcp__agentpit__run_ensemble   — fan out in parallel. Args: {\"members\":[\"<id>\",...],\"prompt\":\"<prompt>\",\"aggregator\":\"<id>\"} (aggregator optional).\n\
\n\
BUDGET: workflow depth 1/3; aim for <= 8 sub-dispatch calls.\n\
The system REJECTS any nested workflow past the depth ceiling. Plan within budget.\n\
\n\
PROCEDURE:\n\
1. Briefly state your plan (the sub-tasks).\n\
2. Call the MCP tools above; read each result; adjust the remaining plan as needed.\n\
3. If a worker fails, note it inline and continue — do not abort the whole run.\n\
4. End with a clearly-labelled SYNTHESIS section integrating all results.\n\
\n\
PARENT RUN ID: run-1   (correlation only; do not modify)\n\
\n\
=== USER GOAL ===\n\
goal\n";
        assert_eq!(text, expected);
    }

    // ---- roles: role-mode prompts ----

    fn sample_roster() -> PromptRoles<'static> {
        PromptRoles {
            roster: Some(
                "  implementer (claude): You implement.\n  reviewer (codex): You review.",
            ),
            persona: None,
        }
    }

    #[test]
    fn role_mode_shell_prompt_teaches_rescue_role_grammar() {
        let agents: [BackendId; 0] = [];
        let text = build_manager_prompt(
            "goal", &agents, "/bin/agentpit", 1, 3, 8, "run-1", None, &sample_roster());
        assert!(text.contains("AVAILABLE ROLES"));
        assert!(text.contains("implementer (claude): You implement."));
        assert!(text.contains("reviewer (codex): You review."));
        assert!(text.contains("'/bin/agentpit' rescue --role <name> \"<sub-task>\""));
        assert!(text.contains("do NOT invent role names"));
        // The flat-backend roster and grammar are gone.
        assert!(!text.contains("AVAILABLE WORKER BACKENDS"));
        assert!(!text.contains("rescue --backend <id>"));
        // Ensemble stays available (backend ids are informational, in parentheses).
        assert!(text.contains("ensemble <id> <id>"));
        // Structure is intact.
        assert!(text.contains("PROCEDURE:"));
        assert!(text.trim_end().ends_with("goal"));
    }

    #[test]
    fn role_mode_mcp_prompt_teaches_dispatch_task_role_arg() {
        let agents: [BackendId; 0] = [];
        let text =
            build_manager_prompt_mcp("goal", &agents, 1, 3, 8, "run-1", None, &sample_roster());
        assert!(text.contains("AVAILABLE ROLES"));
        assert!(text.contains("{\"role\":\"<name>\",\"task\":\"<sub-task>\"}"));
        assert!(text.contains("never pass both"));
        assert!(!text.contains("AVAILABLE WORKER BACKENDS"));
        assert!(!text.contains("run ONE backend"));
        // list_backends / run_ensemble remain.
        assert!(text.contains("mcp__agentpit__list_backends"));
        assert!(text.contains("mcp__agentpit__run_ensemble"));
    }

    #[test]
    fn manager_persona_block_is_injected_in_both_modes() {
        let agents = [BackendId::Gemini];
        let roles = PromptRoles {
            roster: None,
            persona: Some("Prefer small, verifiable steps.\n"),
        };
        let shell = build_manager_prompt(
            "goal", &agents, "/bin/agentpit", 1, 3, 8, "run-1", None, &roles);
        let mcp = build_manager_prompt_mcp("goal", &agents, 1, 3, 8, "run-1", None, &roles);
        for text in [&shell, &mcp] {
            assert!(text.contains("MANAGER PERSONA (from [workflow.roles.manager]):"));
            assert!(text.contains("Prefer small, verifiable steps."));
            // Persona composes with the legacy roster (persona-only manager role).
            assert!(text.contains("AVAILABLE WORKER BACKENDS: gemini"));
        }
    }

    // ---- roles: roster + manager resolution helpers ----

    fn role_map(entries: &[(&str, &[BackendId], Option<&str>)]) -> BTreeMap<String, RoleConfig> {
        entries
            .iter()
            .map(|(name, backends, prompt)| {
                (
                    name.to_string(),
                    RoleConfig {
                        backends: backends.to_vec(),
                        prompt: prompt.map(str::to_string),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn roster_is_none_without_worker_roles() {
        assert_eq!(build_role_roster(&BTreeMap::new(), &[]).unwrap(), None);
        // A manager-only config is still legacy mode for the roster.
        let manager_only = role_map(&[("manager", &[BackendId::Claude], None)]);
        assert_eq!(
            build_role_roster(&manager_only, &[BackendId::Claude]).unwrap(),
            None
        );
    }

    #[test]
    fn roster_renders_resolved_roles_and_skips_unresolvable_ones() {
        let roles = role_map(&[
            ("implementer", &[BackendId::Claude], Some("You implement.")),
            ("reviewer", &[BackendId::Codex], Some("You review.")),
        ]);
        // Codex unavailable → reviewer is skipped with a reason, implementer survives.
        let roster = build_role_roster(&roles, &[BackendId::Claude]).unwrap().unwrap();
        assert_eq!(roster.names, vec!["implementer"]);
        assert!(roster.lines.contains("implementer (claude): You implement."));
        assert_eq!(roster.skipped.len(), 1);
        assert_eq!(roster.skipped[0].0, "reviewer");
        assert!(roster.skipped[0].1.contains("codex"));
    }

    #[test]
    fn roster_errors_when_every_worker_role_is_unresolvable() {
        let roles = role_map(&[("reviewer", &[BackendId::Codex], None)]);
        let err = build_role_roster(&roles, &[]).unwrap_err().to_string();
        assert!(err.contains("no configured worker role could be resolved"));
        assert!(err.contains("reviewer"));
    }

    #[test]
    fn manager_resolution_order_explicit_then_role_then_config_then_default() {
        let roles = role_map(&[(
            "manager",
            &[BackendId::Codex],
            Some("plan tightly"),
        )]);
        // Explicit CLI arg wins over the role; the persona still applies.
        let (backend, persona) = resolve_manager_backend(
            Some(BackendId::Claude),
            &roles,
            Some(BackendId::Claude),
            BackendId::Antigravity,
        )
        .unwrap();
        assert_eq!(backend, BackendId::Claude);
        assert_eq!(persona.as_deref(), Some("plan tightly"));
        // The role wins over [workflow].manager_backend.
        let (backend, _) =
            resolve_manager_backend(None, &roles, Some(BackendId::Claude), BackendId::Antigravity)
                .unwrap();
        assert_eq!(backend, BackendId::Codex);
        // No role → config → default.
        let empty = BTreeMap::new();
        let (backend, persona) =
            resolve_manager_backend(None, &empty, Some(BackendId::Claude), BackendId::Antigravity)
                .unwrap();
        assert_eq!(backend, BackendId::Claude);
        assert_eq!(persona, None);
        let (backend, _) =
            resolve_manager_backend(None, &empty, None, BackendId::Antigravity).unwrap();
        assert_eq!(backend, BackendId::Antigravity);
    }

    #[test]
    fn persona_only_manager_role_keeps_legacy_backend_but_carries_persona() {
        let roles = role_map(&[("manager", &[], Some("persona only"))]);
        let (backend, persona) =
            resolve_manager_backend(None, &roles, Some(BackendId::Codex), BackendId::Antigravity)
                .unwrap();
        assert_eq!(backend, BackendId::Codex);
        assert_eq!(persona.as_deref(), Some("persona only"));
    }

    #[test]
    fn manager_role_with_unsupported_backends_propagates_the_error() {
        // Gemini cannot manage (is_supported_manager: claude|codex only).
        let roles = role_map(&[("manager", &[BackendId::Gemini], None)]);
        assert!(
            resolve_manager_backend(None, &roles, None, BackendId::Antigravity).is_err()
        );
    }
}
