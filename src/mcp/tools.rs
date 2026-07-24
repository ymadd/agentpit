//! The MCP tool surface backing `agentpit mcp serve`.
//!
//! Wraps the existing [`dispatch`] / ensemble / workflow machinery in four MCP tools so a
//! workflow manager (or any MCP client) can orchestrate via structured tool calls instead of
//! shelling out to `agentpit`:
//!
//! - `list_backends` — the available backends plus their transport and auth state.
//! - `dispatch_task` — run ONE backend on a task and return its output.
//! - `run_ensemble` — fan a prompt to several backends in parallel, then optionally aggregate.
//! - `run_workflow` — launch a whole model-driven workflow (a manager decomposes the goal,
//!   dispatches sub-tasks to workers, and returns a final synthesis) by reusing the same
//!   [`crate::cli::workflow::run_capture`] core the `agentpit workflow` CLI uses.
//!
//! Each tool parses the caller-supplied backend id, runs the work under a fresh
//! [`CancellationToken`], clamps the returned text to [`MAX_MEMBER_PROMPT_BYTES`], and surfaces
//! auth-failure / dispatch errors as structured tool errors (`is_error: true`) rather than
//! panicking. The bound matches the ensemble aggregator's so a verbose backend can't blow the
//! manager's context window.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use rmcp::{
    ErrorData as McpError, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::auth::check_auth;
use crate::cli::ensemble::{
    MAX_MEMBER_PROMPT_BYTES, MemberOutcome, build_aggregator_prompt, clamp_for_prompt,
    dispatch_to_outcome, render_concatenated,
};
use crate::dispatch::{Registries, dispatch, resolve_transport};
use crate::events::{LegStatus, RunKind, RunLogger};
use crate::types::BackendId;

/// Parameters for the `dispatch_task` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DispatchTaskRequest {
    /// Backend id to run (claude | codex | gemini | antigravity | opencode). Exactly one of
    /// `backend` or `role` must be provided.
    #[serde(default)]
    pub backend: Option<String>,
    /// Configured `[workflow.roles.<name>]` role to dispatch to; the role resolves which backend
    /// plays it and prepends its persona to the task. Exactly one of `backend` or `role`.
    #[serde(default)]
    pub role: Option<String>,
    /// Optional model to pin (e.g. "opus"). Overrides the role's configured model.
    #[serde(default)]
    pub model: Option<String>,
    /// The task / prompt to give the backend.
    pub task: String,
}

/// Parameters for the `run_ensemble` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RunEnsembleRequest {
    /// Backend ids to fan out to in parallel.
    pub members: Vec<String>,
    /// The prompt sent to every member.
    pub prompt: String,
    /// Optional backend id to synthesize the members' responses into one answer.
    #[serde(default)]
    pub aggregator: Option<String>,
    /// Optional model to pin for every member + the aggregator (e.g. "opus").
    #[serde(default)]
    pub model: Option<String>,
}

/// Parameters for the `run_workflow` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RunWorkflowRequest {
    /// The high-level goal for the manager to decompose and orchestrate.
    pub goal: String,
    /// Named workflow type from `[workflow.types.<name>]` (a preset roster + brief + knobs).
    /// Omit to run the base `[workflow]`.
    #[serde(default, rename = "type")]
    pub workflow_type: Option<String>,
    /// Manager backend (claude|codex). Defaults to config/default backend.
    #[serde(default)]
    pub manager: Option<String>,
    /// Optional model to pin for the manager (e.g. "opus"). Overrides roles.manager.model.
    #[serde(default)]
    pub model: Option<String>,
    /// Worker backends the manager may dispatch to. Defaults to all available minus the manager.
    #[serde(default)]
    pub agents: Option<Vec<String>>,
    /// Recursion depth ceiling (clamped 1..=32).
    #[serde(default)]
    pub max_depth: Option<u32>,
    /// Route the manager through the MCP channel instead of shell-out (claude only).
    #[serde(default)]
    pub use_mcp: Option<bool>,
}

/// Parameters for the `ask_human` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct AskHumanRequest {
    /// The question to put to the human. Pre-frame it as a closed decision.
    pub prompt: String,
    /// Optional explicit options the human picks from (e.g. ["A", "B"]).
    #[serde(default)]
    pub options: Vec<String>,
    /// "blocking" (a worker is stalled until answered) or "review" (nothing blocked).
    /// Anything else defaults to "review".
    #[serde(default)]
    pub kind: Option<String>,
    /// Seconds to wait before returning HUMAN_UNAVAILABLE. Omit or 0 for the default (180s),
    /// capped at 600s.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// Parameters for the `post_note` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PostNoteRequest {
    /// The note body — the context being handed off, or the board entry.
    pub body: String,
    /// "handoff" (1→1 context pass, default) or "board" (shared scratch entry).
    #[serde(default)]
    pub kind: Option<String>,
    /// The backend that authored this note (e.g. the handed-off worker). Omit for a manager post.
    #[serde(default)]
    pub from: Option<String>,
}

/// Parameters for the `refute` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RefuteRequest {
    /// The stuck candidate to put under adversarial scrutiny.
    pub candidate: String,
    /// The sub-task the candidate was meant to achieve (the critic/defender's target).
    pub task: String,
    /// Backend that produces the critique. Defaults to the adversarial-review primary.
    #[serde(default)]
    pub critic: Option<String>,
    /// Backend that produces the defense. Defaults to a backend distinct from the critic.
    #[serde(default)]
    pub defender: Option<String>,
}

/// The agentpit MCP tool handler. Holds a shared [`Registries`], the working directory the
/// dispatched backends run in, the configured `[workflow.roles.*]` map (for `dispatch_task`'s
/// `role` argument), plus the generated [`ToolRouter`].
#[derive(Clone)]
pub struct AgentpitTools {
    regs: Arc<Registries>,
    cwd: PathBuf,
    roles: Arc<std::collections::BTreeMap<String, crate::config::RoleConfig>>,
    tool_router: ToolRouter<Self>,
}

impl AgentpitTools {
    /// Build the tool handler over a shared registry set and working directory, with no roles
    /// configured (role dispatches then fail with "no worker roles are configured").
    pub fn new(regs: Arc<Registries>, cwd: PathBuf) -> Self {
        Self {
            regs,
            cwd,
            roles: Arc::new(std::collections::BTreeMap::new()),
            tool_router: Self::tool_router(),
        }
    }

    /// Attach the configured `[workflow.roles.*]` map so `dispatch_task {role}` can resolve.
    pub fn with_roles(
        mut self,
        roles: std::collections::BTreeMap<String, crate::config::RoleConfig>,
    ) -> Self {
        self.roles = Arc::new(roles);
        self
    }
}

#[tool_router(router = tool_router)]
impl AgentpitTools {
    /// List the backends agentpit can dispatch to, with transport and auth state.
    #[tool(
        name = "list_backends",
        description = "List the backends agentpit can dispatch to, with their transport and auth state."
    )]
    async fn list_backends(&self) -> Result<CallToolResult, McpError> {
        let mut backends: Vec<BackendId> = self.regs.available().into_iter().collect();
        backends.sort();
        // Resolve transport (a cheap sync lookup) up front, then run the per-backend auth checks
        // concurrently: `check_auth` for codex spawns a child process, so a serial loop would pay
        // each backend's latency in series. Mirrors `cli::ensemble::preflight`.
        let mut handles = Vec::with_capacity(backends.len());
        for b in backends {
            let transport = resolve_transport(b, &self.regs)
                .map(|t| t.as_str())
                .unwrap_or("none");
            handles.push((
                b,
                transport,
                tokio::spawn(async move { check_auth(b).await }),
            ));
        }
        let mut lines = Vec::with_capacity(handles.len());
        for (b, transport, handle) in handles {
            let auth_state = match handle.await {
                Ok(auth) if auth.ok => "ok".to_string(),
                Ok(auth) => format!("missing — {}", auth.hint),
                Err(_) => "unknown — auth check task failed".to_string(),
            };
            lines.push(format!("{b}\ttransport={transport}\tauth={auth_state}"));
        }
        let body = if lines.is_empty() {
            "(no backends available)".to_string()
        } else {
            lines.join("\n")
        };
        Ok(CallToolResult::success(vec![Content::text(body)]))
    }

    /// Run ONE backend (or configured role) on a task and return its output.
    #[tool(
        name = "dispatch_task",
        description = "Run ONE backend agent on a task and return its output. Address it either by backend id ({\"backend\":\"<id>\"}) or by a configured workflow role ({\"role\":\"<name>\"} — the role resolves its backend and prepends its persona); exactly one of the two, never both."
    )]
    async fn dispatch_task(
        &self,
        Parameters(req): Parameters<DispatchTaskRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Exactly one of backend|role addresses the dispatch; both or neither is ambiguous and a
        // structured error, mirroring the CLI's `--role`/`--backend` mutual exclusion.
        let (backend, task, model, role) = match (&req.backend, &req.role) {
            (Some(_), Some(_)) => {
                return Ok(tool_error(
                    "'backend' and 'role' are mutually exclusive; pass exactly one".to_string(),
                ));
            }
            (None, None) => {
                return Ok(tool_error(
                    "dispatch_task requires either 'backend' or 'role'".to_string(),
                ));
            }
            (Some(b), None) => match b.parse::<BackendId>() {
                Ok(b) => (b, req.task, req.model.clone(), None),
                Err(e) => return Ok(tool_error(format!("unknown backend '{b}': {e}"))),
            },
            (None, Some(role_name)) => {
                let available: Vec<BackendId> = self.regs.available().into_iter().collect();
                match crate::workflow::roles::resolve_role(role_name, &self.roles, &available) {
                    Ok(resolved) => {
                        let wrapped = crate::workflow::roles::persona_task(
                            &resolved.name,
                            resolved.prompt.as_deref(),
                            &req.task,
                        );
                        // Explicit `model` wins over the role's configured model (no backend
                        // default here — the MCP server holds roles, not the [backends] table).
                        let model = crate::workflow::roles::resolve_model(
                            req.model.as_deref(),
                            resolved.model.as_deref(),
                            None,
                        );
                        (resolved.backend, wrapped, model, Some(resolved.name))
                    }
                    Err(e) => return Ok(tool_error(format!("{e:#}"))),
                }
            }
        };
        let cancel = CancellationToken::new();
        // Surface single MCP dispatches in the dashboard swarm, like `agentpit rescue` — this is
        // also how a `--use-mcp` workflow manager's per-worker dispatches become visible. A role
        // dispatch logs under its RESOLVED backend + carries the role so the dashboard tree can
        // label it "reviewer · codex" rather than just the backend.
        let logger =
            RunLogger::start_with_role(RunKind::Rescue, &[backend], &self.cwd, role.as_deref());
        // MCP dispatches pin their backend explicitly (by id or via a role) — no router involved.
        logger.route_decided(
            backend,
            if role.is_some() { "role" } else { "explicit" },
            None,
            None,
            None,
            &task,
        );
        let outcome = dispatch_member_logged(
            backend,
            task,
            self.cwd.clone(),
            cancel,
            self.regs.clone(),
            logger.clone(),
            model,
        )
        .await;
        logger.finished(if outcome.output.is_some() {
            LegStatus::Ok
        } else {
            LegStatus::Error
        });
        Ok(outcome_to_result(&outcome))
    }

    /// Fan a prompt to several backends in parallel, then optionally aggregate.
    #[tool(
        name = "run_ensemble",
        description = "Fan a prompt to multiple backends in parallel, then optionally synthesize their responses with an aggregator backend."
    )]
    async fn run_ensemble(
        &self,
        Parameters(req): Parameters<RunEnsembleRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Parse backend ids up front (a typo is a clear error, not a silent skip) and dedupe
        // while preserving order: running the same backend twice on one prompt is wasteful, and
        // deduping bounds the fan-out to the distinct known backends so a client cannot ask us to
        // spawn thousands of identical processes.
        let mut members = Vec::with_capacity(req.members.len());
        let mut seen = HashSet::new();
        for m in &req.members {
            match m.parse::<BackendId>() {
                Ok(b) => {
                    if seen.insert(b) {
                        members.push(b);
                    }
                }
                Err(e) => return Ok(tool_error(format!("unknown backend '{m}': {e}"))),
            }
        }
        if members.is_empty() {
            return Ok(tool_error(
                "run_ensemble requires at least one member backend".to_string(),
            ));
        }
        let aggregator = match &req.aggregator {
            Some(a) => match a.parse::<BackendId>() {
                Ok(b) => Some(b),
                Err(e) => {
                    return Ok(tool_error(format!("unknown aggregator backend '{a}': {e}")));
                }
            },
            None => None,
        };

        // Fan out members concurrently into a JoinSet. A fresh root token scopes cancellation to
        // this tool call; the JoinSet aborts every still-running task when it is dropped, so if
        // this request future is dropped (client disconnect / shutdown) the in-flight backend
        // dispatches are torn down (their child processes are `kill_on_drop`) instead of leaking.
        let cancel = CancellationToken::new();
        // Surface the fan-out in the dashboard swarm as an `ensemble` run, like the CLI ensemble —
        // each member streams to the run's capture file. Started before the loop moves `members`.
        let logger = RunLogger::start(RunKind::Ensemble, &members, &self.cwd);
        // Same shape as the CLI ensemble: no router ran; the line carries the task_hash and
        // names the aggregator — else the first member — as "the" backend.
        if let Some(primary) = aggregator.or_else(|| members.first().copied()) {
            logger.route_decided(primary, "ensemble", None, None, None, &req.prompt);
        }
        let mut set = tokio::task::JoinSet::new();
        for b in members {
            let prompt = req.prompt.clone();
            let cwd = self.cwd.clone();
            let regs = self.regs.clone();
            let cancel = cancel.clone();
            let logger = logger.clone();
            let model = req.model.clone();
            set.spawn(async move {
                dispatch_member_logged(b, prompt, cwd, cancel, regs, logger, model).await
            });
        }
        let mut outcomes: Vec<MemberOutcome> = Vec::with_capacity(set.len());
        while let Some(joined) = set.join_next().await {
            // `dispatch_member_logged` is panic-free and maps every failure to a MemberOutcome, so a
            // JoinError only arises from a panic/abort; surface the survivors rather than failing.
            if let Ok(o) = joined {
                outcomes.push(o);
            }
        }
        // JoinSet yields in completion order; sort by backend so the rendered output is stable.
        outcomes.sort_by_key(|o| o.backend);

        let any_success = outcomes.iter().any(|o| o.output.is_some());
        let mut combined = render_concatenated(&outcomes);

        if let Some(agg) = aggregator {
            if !any_success {
                combined.push_str("\n\n=== aggregator skipped ===\nno members succeeded");
            } else if resolve_transport(agg, &self.regs).is_none() {
                combined.push_str(&format!(
                    "\n\n=== aggregator skipped ===\n{agg} not registered"
                ));
            } else {
                let agg_prompt = build_aggregator_prompt(&req.prompt, &outcomes);
                let cancel = CancellationToken::new();
                logger.member_started(agg, true);
                let started = Instant::now();
                let on_chunk = crate::events::output_streamer(logger.run_id(), agg, true);
                match dispatch(
                    agg,
                    &agg_prompt,
                    &self.cwd,
                    cancel,
                    on_chunk,
                    &self.regs,
                    req.model.as_deref(),
                )
                .await
                {
                    Ok(res) if res.auth_failed => {
                        logger.member_finished(
                            agg,
                            true,
                            LegStatus::Error,
                            started.elapsed().as_millis() as u64,
                            None,
                            Some("auth failure during execution".into()),
                        );
                        combined.push_str(&format!(
                            "\n\n=== aggregator failed ===\n{agg}: auth failure during execution"
                        ));
                    }
                    Ok(res) => {
                        logger.member_finished(
                            agg,
                            true,
                            LegStatus::Ok,
                            started.elapsed().as_millis() as u64,
                            Some(res.output.len()),
                            None,
                        );
                        combined.push_str(&format!(
                            "\n\n=== aggregator [{agg}] (transport={}) ===\n{}",
                            res.transport.as_str(),
                            res.output.trim()
                        ));
                    }
                    Err(err) => {
                        logger.member_finished(
                            agg,
                            true,
                            LegStatus::Error,
                            started.elapsed().as_millis() as u64,
                            None,
                            Some(format!("{err:#}")),
                        );
                        combined.push_str(&format!("\n\n=== aggregator failed ===\n{agg}: {err:#}"))
                    }
                }
            }
        }

        logger.finished(if any_success {
            LegStatus::Ok
        } else {
            LegStatus::Error
        });

        let clamped = clamp_for_prompt(&combined, MAX_MEMBER_PROMPT_BYTES);
        let content = vec![Content::text(clamped)];
        if any_success {
            Ok(CallToolResult::success(content))
        } else {
            Ok(CallToolResult::error(content))
        }
    }

    /// Launch a whole model-driven workflow and return the manager's final synthesis.
    #[tool(
        name = "run_workflow",
        description = "Launch a model-driven workflow: a manager backend (claude|codex) decomposes the goal, dispatches sub-tasks to worker backends, and returns a final synthesis."
    )]
    async fn run_workflow(
        &self,
        Parameters(req): Parameters<RunWorkflowRequest>,
    ) -> Result<CallToolResult, McpError> {
        // Parse the optional manager id (a typo is a clear error, not a silent default).
        let manager = match &req.manager {
            Some(m) => match m.parse::<BackendId>() {
                Ok(b) => Some(b),
                Err(e) => return Ok(tool_error(format!("unknown manager backend '{m}': {e}"))),
            },
            None => None,
        };
        // Parse the optional worker roster up front so an unknown entry surfaces as a structured
        // error rather than being discovered mid-run.
        let agents = match &req.agents {
            Some(list) => {
                let mut parsed = Vec::with_capacity(list.len());
                for a in list {
                    match a.parse::<BackendId>() {
                        Ok(b) => parsed.push(b),
                        Err(e) => {
                            return Ok(tool_error(format!("unknown agent backend '{a}': {e}")));
                        }
                    }
                }
                Some(parsed)
            }
            None => None,
        };

        // Reuse the same launch core the CLI uses; a fresh root token scopes cancellation to this
        // tool call, and the no-op sink suppresses streaming (we return the captured output). The
        // recursion-depth guard, manager validation, and auth preflight all live inside
        // `run_capture`, so auth / depth / unsupported-manager failures come back as structured
        // tool errors below rather than panicking.
        match crate::cli::workflow::run_capture(
            req.goal,
            req.workflow_type,
            manager,
            agents,
            req.max_depth,
            req.use_mcp.unwrap_or(false),
            req.model,
            self.cwd.clone(),
            CancellationToken::new(),
            noop_sink(),
        )
        .await
        {
            Ok(output) => Ok(CallToolResult::success(vec![Content::text(
                clamp_for_prompt(&output, MAX_MEMBER_PROMPT_BYTES),
            )])),
            Err(e) => Ok(tool_error(format!("{e:#}"))),
        }
    }

    /// Ask the supervising human a question and block for an answer. The `cancel` token is
    /// injected by rmcp from the request context, so a client `CancelledNotification` (or the
    /// serve loop tearing down) aborts the poll promptly instead of waiting out the timeout.
    #[tool(
        name = "ask_human",
        description = "Ask the supervising HUMAN a question and block for an answer. ONLY the workflow manager may call this — workers cannot. Returns the human's answer, or the sentinel HUMAN_UNAVAILABLE if no one answers before the timeout (then proceed with the safe, conservative, reversible choice and note it). Use SPARINGLY: only at a genuine decision fork or before a destructive / irreversible action."
    )]
    async fn ask_human(
        &self,
        Parameters(req): Parameters<AskHumanRequest>,
        cancel: CancellationToken,
    ) -> Result<CallToolResult, McpError> {
        let areq = crate::ask::AskRequest {
            prompt: req.prompt,
            options: req.options,
            kind: crate::ask::AskKind::parse_or_default(req.kind.as_deref()),
            timeout_secs: req.timeout_secs.unwrap_or(0),
        };
        match crate::ask::ask(areq, cancel).await {
            crate::ask::AskOutcome::Answered(a) => {
                Ok(CallToolResult::success(vec![Content::text(
                    clamp_for_prompt(&a, MAX_MEMBER_PROMPT_BYTES),
                )]))
            }
            // SUCCESS text, never `tool_error` — a timeout must not look like a failure, or the
            // manager may abort instead of proceeding with the safe choice on HUMAN_UNAVAILABLE.
            crate::ask::AskOutcome::Unavailable => {
                Ok(CallToolResult::success(vec![Content::text(
                    crate::ask::HUMAN_UNAVAILABLE.to_string(),
                )]))
            }
        }
    }

    /// Append a durable conversation-layer note (① handoff / ③ shared board) to the run transcript.
    /// Structurally manager-only: workers have no MCP channel, so — like `ask_human` — no token
    /// gate is needed. The note is fire-and-forget onto the run's `events.jsonl`; it returns once
    /// recorded and is never waited on.
    #[tool(
        name = "post_note",
        description = "Record a durable note onto the workflow transcript: a 1→1 handoff (kind=\"handoff\", the default — pass the context the next worker needs, optionally with from=<worker that produced it>) or a shared-board entry (kind=\"board\"). Use a handoff when one worker's result must seed the next sub-task; use the board for a fact several sub-tasks will reuse. Fire-and-forget — it does not block."
    )]
    async fn post_note(
        &self,
        Parameters(req): Parameters<PostNoteRequest>,
    ) -> Result<CallToolResult, McpError> {
        let from = match &req.from {
            Some(f) => match f.parse::<BackendId>() {
                Ok(b) => Some(b),
                Err(e) => return Ok(tool_error(format!("unknown backend '{f}': {e}"))),
            },
            None => None,
        };
        // The note must attach to the manager's run. The workflow sets AGENTPIT_PARENT_RUN_ID on
        // the manager leg the MCP server runs under; without it there is no transcript to write to.
        let run_id = std::env::var(crate::workflow::guard::ENV_PARENT_RUN_ID)
            .ok()
            .filter(|s| !s.is_empty());
        let Some(run_id) = run_id else {
            return Ok(tool_error(
                "post_note requires a workflow run context (AGENTPIT_PARENT_RUN_ID is unset)"
                    .into(),
            ));
        };
        let kind = crate::workflow::converse::normalize_kind(req.kind.as_deref());
        crate::events::RunLogger::adopt(run_id).note(from, &kind, &req.body);
        Ok(CallToolResult::success(vec![Content::text(format!(
            "recorded {kind} note ({} bytes)",
            req.body.len()
        ))]))
    }

    /// Run one ④ refutation pass over a stuck candidate: an adversarial critic, then a defender
    /// carrying that critique, returned together for the manager to adjudicate. Advisory — a failed
    /// leg is reported in the text, not as a tool error, so it never aborts the manager.
    #[tool(
        name = "refute",
        description = "Stress-test a STUCK sub-task before discarding it: dispatch an adversarial critic at the candidate, then a defender that rebuts/fixes it, and return both for YOU (the manager) to adjudicate — ADOPT the revised candidate, KEEP the original, or DISCARD and re-plan. One depth-guarded pass, not a loop. Run this ONCE on a candidate you are about to throw away."
    )]
    async fn refute(
        &self,
        Parameters(req): Parameters<RefuteRequest>,
    ) -> Result<CallToolResult, McpError> {
        let critic = match parse_opt_backend(req.critic.as_deref(), "critic") {
            Ok(b) => b,
            Err(msg) => return Ok(tool_error(msg)),
        };
        let defender = match parse_opt_backend(req.defender.as_deref(), "defender") {
            Ok(b) => b,
            Err(msg) => return Ok(tool_error(msg)),
        };
        // Availability comes from the server's live registry; the preferred pairing honors the
        // user's adversarial-review config (falling back to none if config cannot be read).
        let available = self.regs.available();
        let preferred = crate::cli::load_context()
            .map(|c| c.loaded.config.ensemble.adversarial_review_members.clone())
            .unwrap_or_default();
        let (critic, defender) =
            match crate::workflow::converse::resolve_pair(critic, defender, &available, &preferred)
            {
                Ok(pair) => pair,
                Err(e) => return Ok(tool_error(format!("{e:#}"))),
            };

        // Surface the refutation in the dashboard swarm as its own adversarial-review run, just like
        // a manager's shell-out to `agentpit rescue` appears as a distinct run rather than vanishing.
        let members = if critic == defender {
            vec![critic]
        } else {
            vec![critic, defender]
        };
        let logger = crate::events::RunLogger::start(
            crate::events::RunKind::AdversarialReview,
            &members,
            &self.cwd,
        );
        // The critic leg carries the refutation; log it as the run's routed backend.
        logger.route_decided(critic, "refute", None, None, None, &req.task);

        let bundle = crate::workflow::converse::run_refute(
            &req.task,
            &req.candidate,
            critic,
            defender,
            &self.cwd,
            &self.regs,
            CancellationToken::new(),
            Some(&logger),
        )
        .await;
        logger.finished(if bundle.critique.is_ok() {
            crate::events::LegStatus::Ok
        } else {
            crate::events::LegStatus::Error
        });
        Ok(CallToolResult::success(vec![Content::text(
            clamp_for_prompt(
                &crate::workflow::converse::render_refute(&bundle),
                MAX_MEMBER_PROMPT_BYTES,
            ),
        )]))
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for AgentpitTools {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "agentpit multi-agent hub. Call list_backends to discover backends, dispatch_task to \
             run one backend on a sub-task, run_ensemble to fan a prompt to several backends in \
             parallel (with an optional aggregator), run_workflow to launch a whole \
             model-driven workflow (a manager decomposes the goal, dispatches sub-tasks to \
             workers, and returns a final synthesis), ask_human to surface a decision to the \
             supervising human and block for an answer (use sparingly — only at a genuine fork \
             or before a destructive action; HUMAN_UNAVAILABLE means proceed with the safe choice), \
             post_note to record a handoff or shared-board entry on the transcript, and refute to \
             stress-test a stuck sub-task (critic → defender) once before discarding it.",
        )
    }
}

/// A no-op streaming sink. `dispatch()` already collects each backend's full stdout into
/// [`crate::dispatch::DispatchResult::output`], so a separate accumulating sink would only
/// duplicate it — the tools read the returned `output` instead.
fn noop_sink() -> Arc<dyn Fn(&str) + Send + Sync> {
    Arc::new(|_chunk: &str| {})
}

/// Build a structured tool error result (`is_error: true`) carrying `msg`.
fn tool_error(msg: String) -> CallToolResult {
    CallToolResult::error(vec![Content::text(msg)])
}

/// Parse an optional backend id, returning a `role`-labelled error message on a typo. `None` maps
/// to `Ok(None)` so an omitted backend defaults downstream rather than erroring.
fn parse_opt_backend(value: Option<&str>, role: &str) -> Result<Option<BackendId>, String> {
    match value {
        Some(v) => v
            .parse::<BackendId>()
            .map(Some)
            .map_err(|e| format!("unknown {role} backend '{v}': {e}")),
        None => Ok(None),
    }
}

/// Run one backend dispatch and capture it as a [`MemberOutcome`], bracketing it with
/// `member_started`/`member_finished` and streaming its chunks to the run's capture file — so an MCP
/// `dispatch_task` / `run_ensemble` shows in the dashboard swarm exactly like the CLI ensemble,
/// including the worker dispatches a `--use-mcp` workflow manager makes, which would otherwise run
/// invisibly. A thin adapter over the shared [`dispatch_to_outcome`] core in `cli::ensemble` (so the
/// not-registered / auth-failure / error mapping stays defined once), mirroring
/// `cli::ensemble::run_one_member` minus the TTY reporting the MCP server has no terminal for. A
/// `not registered` outcome is recorded as `Skipped` (the backend was never dispatched), not an
/// execution error.
async fn dispatch_member_logged(
    backend: BackendId,
    prompt: String,
    cwd: PathBuf,
    cancel: CancellationToken,
    regs: Arc<Registries>,
    logger: RunLogger,
    model: Option<String>,
) -> MemberOutcome {
    logger.member_started(backend, false);
    let started = Instant::now();
    let on_chunk = crate::events::output_streamer(logger.run_id(), backend, false);
    let outcome = dispatch_to_outcome(
        backend,
        &prompt,
        &cwd,
        cancel,
        on_chunk,
        &regs,
        model.as_deref(),
    )
    .await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match &outcome.error {
        None => {
            let chars = outcome.output.as_ref().map(String::len).unwrap_or(0);
            logger.member_finished(backend, false, LegStatus::Ok, elapsed_ms, Some(chars), None);
        }
        Some(err) => {
            let status = if err == "not registered" {
                LegStatus::Skipped
            } else {
                LegStatus::Error
            };
            logger.member_finished(backend, false, status, elapsed_ms, None, Some(err.clone()));
        }
    }
    outcome
}

/// Convert a single member outcome into a tool result: a success carrying the clamped output,
/// or a structured error carrying the failure reason.
fn outcome_to_result(o: &MemberOutcome) -> CallToolResult {
    if let Some(out) = &o.output {
        CallToolResult::success(vec![Content::text(clamp_for_prompt(
            out.trim(),
            MAX_MEMBER_PROMPT_BYTES,
        ))])
    } else {
        let err = o.error.clone().unwrap_or_else(|| "no output".into());
        tool_error(format!("[{}] {err}", o.backend))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::{ExecAdapter, ExecSpec};

    // Mirror the DummyExec used in dispatch.rs: a backend that runs `true` (exit 0, no output).
    struct DummyExec;
    impl ExecAdapter for DummyExec {
        fn id(&self) -> BackendId {
            BackendId::Gemini
        }
        fn build_spec(&self, _task: &str, _model: Option<&str>) -> ExecSpec {
            ExecSpec {
                command: "true".into(),
                args: vec![],
                env: vec![],
                stdin_input: None,
            }
        }
    }

    fn tools() -> AgentpitTools {
        let mut regs = Registries::empty();
        regs.execs.insert(BackendId::Gemini, Box::new(DummyExec));
        AgentpitTools::new(Arc::new(regs), std::env::temp_dir())
    }

    /// RAII guard that isolates a test's event writes into a throwaway `XDG_STATE_HOME`, so a test
    /// that dispatches a real backend (now routed through a `RunLogger`) can't append phantom runs to
    /// the developer's real events.jsonl. Mirrors the inline `ask_human` / `post_note` pattern:
    /// serialize on [`STATE_ENV_LOCK`](crate::ask::STATE_ENV_LOCK), point the state dir at a temp dir,
    /// and clear it again on drop — all while the lock is still held. Bind it for the whole test
    /// (`let _g = isolated_state_dir();`); do NOT call it from inside `tools()`, which some tests
    /// invoke while already holding the lock (re-locking the non-reentrant mutex would deadlock).
    struct StateDirGuard {
        _lock: std::sync::MutexGuard<'static, ()>,
        _tmp: tempfile::TempDir,
    }
    impl Drop for StateDirGuard {
        fn drop(&mut self) {
            // SAFETY: the `_lock` field is still alive (drops after this), so we remove the var while
            // single-threaded under STATE_ENV_LOCK, exactly as the inline tests do at their end.
            unsafe {
                std::env::remove_var("XDG_STATE_HOME");
            }
        }
    }
    fn isolated_state_dir() -> StateDirGuard {
        let lock = crate::ask::STATE_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: single-threaded under STATE_ENV_LOCK for the guard's lifetime.
        unsafe {
            std::env::set_var("XDG_STATE_HOME", tmp.path());
        }
        StateDirGuard {
            _lock: lock,
            _tmp: tmp,
        }
    }

    #[tokio::test]
    async fn list_backends_reports_available_set() {
        let res = tools().list_backends().await.unwrap();
        assert_eq!(res.is_error, Some(false));
        let text = res
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("gemini"), "got: {text}");
        assert!(text.contains("transport=exec"), "got: {text}");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn dispatch_task_runs_registered_backend() {
        // The dispatch now emits run events; isolate them into a throwaway state dir.
        let _g = isolated_state_dir();
        let res = tools()
            .dispatch_task(Parameters(DispatchTaskRequest {
                model: None,
                backend: Some("gemini".into()),
                role: None,
                task: "noop".into(),
            }))
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(false), "expected success, got: {res:?}");
    }

    #[tokio::test]
    async fn dispatch_task_unknown_backend_is_structured_error_not_panic() {
        let res = tools()
            .dispatch_task(Parameters(DispatchTaskRequest {
                model: None,
                backend: Some("imaginary".into()),
                role: None,
                task: "noop".into(),
            }))
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(true));
        let text = res
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("unknown backend"), "got: {text}");
    }

    fn tools_with_reviewer_role() -> AgentpitTools {
        let mut roles = std::collections::BTreeMap::new();
        roles.insert(
            "reviewer".to_string(),
            crate::config::RoleConfig {
                backends: vec![BackendId::Gemini],
                prompt: Some("You review.".into()),
                model: None,
            },
        );
        tools().with_roles(roles)
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn dispatch_task_role_runs_resolved_backend() {
        let _g = isolated_state_dir();
        let res = tools_with_reviewer_role()
            .dispatch_task(Parameters(DispatchTaskRequest {
                model: None,
                backend: None,
                role: Some("reviewer".into()),
                task: "noop".into(),
            }))
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(false), "expected success, got: {res:?}");
        // The dispatched sub-run's RunStarted must carry the resolved role so the dashboard tree
        // can label it by role — guards the start_with_role wiring at this call site.
        let log = std::fs::read_to_string(agentpit_events::events_path()).unwrap_or_default();
        let started = log
            .lines()
            .find(|l| l.contains("run_started"))
            .expect("a run_started line was written");
        assert!(
            started.contains("\"role\":\"reviewer\""),
            "role not emitted: {started}"
        );
    }

    #[tokio::test]
    async fn dispatch_task_unknown_role_is_structured_error() {
        let res = tools_with_reviewer_role()
            .dispatch_task(Parameters(DispatchTaskRequest {
                model: None,
                backend: None,
                role: Some("ghost".into()),
                task: "noop".into(),
            }))
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(true));
        let text = res
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("unknown role 'ghost'"), "got: {text}");
        assert!(text.contains("reviewer"), "got: {text}");
    }

    #[tokio::test]
    async fn dispatch_task_backend_and_role_together_is_structured_error() {
        let res = tools_with_reviewer_role()
            .dispatch_task(Parameters(DispatchTaskRequest {
                model: None,
                backend: Some("gemini".into()),
                role: Some("reviewer".into()),
                task: "noop".into(),
            }))
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(true));
        let text = res
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("mutually exclusive"), "got: {text}");
    }

    #[tokio::test]
    async fn dispatch_task_neither_backend_nor_role_is_structured_error() {
        let res = tools()
            .dispatch_task(Parameters(DispatchTaskRequest {
                model: None,
                backend: None,
                role: None,
                task: "noop".into(),
            }))
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(true));
        let text = res
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("either 'backend' or 'role'"), "got: {text}");
    }

    #[tokio::test]
    async fn run_ensemble_rejects_empty_members() {
        let res = tools()
            .run_ensemble(Parameters(RunEnsembleRequest {
                model: None,
                members: vec![],
                prompt: "hi".into(),
                aggregator: None,
            }))
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(true));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn run_ensemble_dedupes_repeated_members() {
        // A client repeating the same backend must not spawn it N times; dedup collapses it to
        // a single member section. The dispatch emits run events; isolate them into a temp state dir.
        let _g = isolated_state_dir();
        let res = tools()
            .run_ensemble(Parameters(RunEnsembleRequest {
                model: None,
                members: vec!["gemini".into(), "gemini".into(), "gemini".into()],
                prompt: "noop".into(),
                aggregator: None,
            }))
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(false));
        let text = res
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(
            text.matches("=== gemini").count(),
            1,
            "deduped ensemble must render exactly one gemini section; got: {text}"
        );
    }

    #[tokio::test]
    async fn run_workflow_unknown_manager_is_structured_error_not_panic() {
        let res = tools()
            .run_workflow(Parameters(RunWorkflowRequest {
                model: None,
                goal: "do something".into(),
                workflow_type: None,
                manager: Some("imaginary".into()),
                agents: None,
                max_depth: None,
                use_mcp: None,
            }))
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(true));
        let text = res
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("unknown manager backend"), "got: {text}");
    }

    #[tokio::test]
    async fn run_workflow_unknown_agent_is_structured_error_not_panic() {
        let res = tools()
            .run_workflow(Parameters(RunWorkflowRequest {
                model: None,
                goal: "do something".into(),
                workflow_type: None,
                manager: None,
                agents: Some(vec!["gemini".into(), "imaginary".into()]),
                max_depth: None,
                use_mcp: None,
            }))
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(true));
        let text = res
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("unknown agent backend"), "got: {text}");
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn ask_human_times_out_to_sentinel_as_success() {
        // Serialize against other state-dir tests; isolate writes to a temp XDG_STATE_HOME.
        let _g = crate::ask::STATE_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: single-threaded under STATE_ENV_LOCK.
        unsafe {
            std::env::set_var("XDG_STATE_HOME", tmp.path());
        }
        let res = tools()
            .ask_human(
                Parameters(AskHumanRequest {
                    prompt: "Proceed?".into(),
                    options: vec![],
                    kind: Some("review".into()),
                    timeout_secs: Some(1),
                }),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        // A timeout must surface as SUCCESS carrying the sentinel, never an error — else the
        // manager may abort instead of proceeding with the safe choice.
        assert_eq!(res.is_error, Some(false));
        let text = res
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(text, "HUMAN_UNAVAILABLE");
        unsafe {
            std::env::remove_var("XDG_STATE_HOME");
        }
    }

    fn text_of(res: &CallToolResult) -> String {
        res.content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn post_note_unknown_from_is_structured_error_not_panic() {
        let res = tools()
            .post_note(Parameters(PostNoteRequest {
                body: "ctx".into(),
                kind: None,
                from: Some("imaginary".into()),
            }))
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(true));
        assert!(
            text_of(&res).contains("unknown backend"),
            "got: {}",
            text_of(&res)
        );
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn post_note_without_run_context_is_error() {
        let _g = crate::ask::STATE_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // SAFETY: single-threaded under STATE_ENV_LOCK.
        unsafe {
            std::env::remove_var(crate::workflow::guard::ENV_PARENT_RUN_ID);
        }
        let res = tools()
            .post_note(Parameters(PostNoteRequest {
                body: "ctx".into(),
                kind: None,
                from: None,
            }))
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(true));
        assert!(text_of(&res).contains("requires a workflow run context"));
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)]
    async fn post_note_records_with_run_context() {
        let _g = crate::ask::STATE_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        // SAFETY: single-threaded under STATE_ENV_LOCK.
        unsafe {
            std::env::set_var("XDG_STATE_HOME", tmp.path());
            std::env::set_var(crate::workflow::guard::ENV_PARENT_RUN_ID, "run-mcp-note");
        }
        let res = tools()
            .post_note(Parameters(PostNoteRequest {
                body: "pass this on".into(),
                kind: Some("board".into()),
                from: Some("gemini".into()),
            }))
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(false), "got: {}", text_of(&res));
        assert!(
            text_of(&res).contains("recorded board note"),
            "got: {}",
            text_of(&res)
        );
        unsafe {
            std::env::remove_var("XDG_STATE_HOME");
            std::env::remove_var(crate::workflow::guard::ENV_PARENT_RUN_ID);
        }
    }

    #[tokio::test]
    async fn refute_unknown_critic_is_structured_error_not_panic() {
        let res = tools()
            .refute(Parameters(RefuteRequest {
                candidate: "x".into(),
                task: "y".into(),
                critic: Some("imaginary".into()),
                defender: None,
            }))
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(true));
        assert!(
            text_of(&res).contains("unknown critic backend"),
            "got: {}",
            text_of(&res)
        );
    }
}
