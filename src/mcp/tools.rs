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
use crate::types::BackendId;

/// Parameters for the `dispatch_task` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct DispatchTaskRequest {
    /// Backend id to run (claude | codex | gemini | antigravity | opencode).
    pub backend: String,
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
}

/// Parameters for the `run_workflow` tool.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RunWorkflowRequest {
    /// The high-level goal for the manager to decompose and orchestrate.
    pub goal: String,
    /// Manager backend (claude|codex). Defaults to config/default backend.
    #[serde(default)]
    pub manager: Option<String>,
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

/// The agentpit MCP tool handler. Holds a shared [`Registries`] and the working directory the
/// dispatched backends run in, plus the generated [`ToolRouter`].
#[derive(Clone)]
pub struct AgentpitTools {
    regs: Arc<Registries>,
    cwd: PathBuf,
    tool_router: ToolRouter<Self>,
}

impl AgentpitTools {
    /// Build the tool handler over a shared registry set and working directory.
    pub fn new(regs: Arc<Registries>, cwd: PathBuf) -> Self {
        Self {
            regs,
            cwd,
            tool_router: Self::tool_router(),
        }
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

    /// Run ONE backend on a task and return its output (or a structured error).
    #[tool(
        name = "dispatch_task",
        description = "Run ONE backend agent on a task and return its output."
    )]
    async fn dispatch_task(
        &self,
        Parameters(req): Parameters<DispatchTaskRequest>,
    ) -> Result<CallToolResult, McpError> {
        let backend = match req.backend.parse::<BackendId>() {
            Ok(b) => b,
            Err(e) => {
                return Ok(tool_error(format!(
                    "unknown backend '{}': {e}",
                    req.backend
                )));
            }
        };
        let cancel = CancellationToken::new();
        let outcome = dispatch_member(
            backend,
            req.task,
            self.cwd.clone(),
            cancel,
            self.regs.clone(),
        )
        .await;
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
        let mut set = tokio::task::JoinSet::new();
        for b in members {
            let prompt = req.prompt.clone();
            let cwd = self.cwd.clone();
            let regs = self.regs.clone();
            let cancel = cancel.clone();
            set.spawn(async move { dispatch_member(b, prompt, cwd, cancel, regs).await });
        }
        let mut outcomes: Vec<MemberOutcome> = Vec::with_capacity(set.len());
        while let Some(joined) = set.join_next().await {
            // `dispatch_member` is panic-free and maps every failure to a MemberOutcome, so a
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
                match dispatch(agg, &agg_prompt, &self.cwd, cancel, noop_sink(), &self.regs).await {
                    Ok(res) if res.auth_failed => combined.push_str(&format!(
                        "\n\n=== aggregator failed ===\n{agg}: auth failure during execution"
                    )),
                    Ok(res) => combined.push_str(&format!(
                        "\n\n=== aggregator [{agg}] (transport={}) ===\n{}",
                        res.transport.as_str(),
                        res.output.trim()
                    )),
                    Err(err) => {
                        combined.push_str(&format!("\n\n=== aggregator failed ===\n{agg}: {err:#}"))
                    }
                }
            }
        }

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
            manager,
            agents,
            req.max_depth,
            req.use_mcp.unwrap_or(false),
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
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for AgentpitTools {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "agentpit multi-agent hub. Call list_backends to discover backends, dispatch_task to \
             run one backend on a sub-task, run_ensemble to fan a prompt to several backends in \
             parallel (with an optional aggregator), and run_workflow to launch a whole \
             model-driven workflow (a manager decomposes the goal, dispatches sub-tasks to \
             workers, and returns a final synthesis).",
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

/// Run one backend dispatch and capture it as a [`MemberOutcome`], with no streaming (the MCP path
/// has no TTY). A thin adapter over the shared [`dispatch_to_outcome`] core in `cli::ensemble`, so
/// the not-registered / auth-failure / error mapping is defined once.
async fn dispatch_member(
    backend: BackendId,
    prompt: String,
    cwd: PathBuf,
    cancel: CancellationToken,
    regs: Arc<Registries>,
) -> MemberOutcome {
    dispatch_to_outcome(backend, &prompt, &cwd, cancel, noop_sink(), &regs).await
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
        fn build_spec(&self, _task: &str) -> ExecSpec {
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
    async fn dispatch_task_runs_registered_backend() {
        let res = tools()
            .dispatch_task(Parameters(DispatchTaskRequest {
                backend: "gemini".into(),
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
                backend: "imaginary".into(),
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

    #[tokio::test]
    async fn run_ensemble_rejects_empty_members() {
        let res = tools()
            .run_ensemble(Parameters(RunEnsembleRequest {
                members: vec![],
                prompt: "hi".into(),
                aggregator: None,
            }))
            .await
            .unwrap();
        assert_eq!(res.is_error, Some(true));
    }

    #[tokio::test]
    async fn run_ensemble_dedupes_repeated_members() {
        // A client repeating the same backend must not spawn it N times; dedup collapses it to
        // a single member section.
        let res = tools()
            .run_ensemble(Parameters(RunEnsembleRequest {
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
                goal: "do something".into(),
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
                goal: "do something".into(),
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
}
