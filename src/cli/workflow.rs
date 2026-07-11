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
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use std::collections::BTreeMap;

use super::{install_ctrlc_cancel, load_context, resolve_cwd, stdout_streamer};
use crate::auth::check_auth;
use crate::config::{HubConfig, RoleConfig, WorkflowSection};
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
    workflow_type: Option<String>,
    manager: Option<BackendId>,
    agents: Option<Vec<BackendId>>,
    max_depth: Option<u32>,
    use_mcp: bool,
    model: Option<String>,
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
        workflow_type,
        manager,
        agents,
        max_depth,
        use_mcp,
        model,
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
    workflow_type: Option<String>,
    manager: Option<BackendId>,
    agents: Option<Vec<BackendId>>,
    max_depth: Option<u32>,
    use_mcp: bool,
    model: Option<String>,
    cwd: PathBuf,
    cancel: CancellationToken,
    terminal_sink: Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<String> {
    let ctx = load_context()?;
    let config = &ctx.loaded.config;

    // 0. Resolve the named workflow TYPE into an effective view over [workflow]: a type is a preset
    //    that overrides the manager backend, knobs, and role roster, and adds a manager BRIEF. No
    //    type (`agentpit workflow "<goal>"`) = the base [workflow] unchanged.
    let eff = resolve_workflow_type(&config.workflow, workflow_type.as_deref())?;

    // 1. Resolve the manager: explicit arg → the type's manager_backend → [workflow.roles.manager]
    //    → [workflow].manager_backend → default backend. The manager role's persona applies
    //    whenever the role exists, even when the backend came from another step.
    let (manager, manager_persona, manager_role_model) = resolve_manager_backend(
        manager,
        eff.type_manager_backend,
        &config.workflow.roles,
        eff.manager_backend,
        config.default.backend,
    )?;
    if !is_supported_manager(manager) {
        anyhow::bail!("supported workflow managers: claude, codex (got: {manager})");
    }
    // The manager's model: explicit `--model` > [workflow.roles.manager].model > the manager
    // backend's `[backends.<id>].model` default > None (the CLI's own default).
    let manager_model = roles::resolve_model(
        model.as_deref(),
        manager_role_model.as_deref(),
        config.backends.get(&manager).and_then(|o| o.model.as_deref()),
    );

    // 1b. MCP mode: the `--use-mcp` flag OR the effective use_mcp knob, but only the claude manager
    //     supports it (codex MCP mode is out of scope). Fall back to shell-out otherwise.
    let use_mcp = use_mcp || eff.use_mcp;
    let mcp_mode = use_mcp && manager == BackendId::Claude;
    if use_mcp && !mcp_mode {
        eprintln!(
            "warning: --use-mcp is only supported for the claude manager; using shell-out mode for {manager}."
        );
    }

    // 2. Resolve the worker roster. With `[workflow.roles.*]` worker roles configured the
    //    roster is ROLES (casting is config-driven); a type may further NARROW it to a subset.
    //    Otherwise the legacy flat backend list: explicit → config → all available minus manager.
    let available: Vec<BackendId> = ctx.regs.available().into_iter().collect();
    let roster = build_role_roster(&config.workflow.roles, &available, eff.role_filter.as_deref())?;
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

    // 3. Resolve the depth ceiling: arg → effective (type → config, default 3), clamped to a hard
    //    upper bound so a hostile `--max-depth` can never push the depth toward `u32::MAX` and wrap.
    let max_depth = guard::clamp_max_depth(max_depth.unwrap_or(eff.max_depth));
    let max_calls = eff.max_calls_per_manager;

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
    let ask = eff.enable_ask_human.then_some(AskTier::High);
    let prompt_roles = PromptRoles {
        roster: roster.as_ref().map(|r| r.lines.as_str()),
        persona: manager_persona.as_deref(),
        brief: eff.brief.as_deref(),
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
    if eff.enable_ask_human {
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
    let type_tag = match &eff.type_name {
        Some(t) => format!("type={t} "),
        None => String::new(),
    };
    terminal_sink(&format!(
        "[workflow {type_tag}manager={manager} depth={depth}/{max_depth} mcp={mcp_mode} {cast}]\n"
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
    let result =
        dispatch(manager, &prompt, &cwd, cancel, on_chunk, &regs, manager_model.as_deref()).await;

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

/// Reserved first-positional token: `agentpit workflow new "<description>"` routes to the workflow
/// designer, so a user-defined type may never be named `new` (it would shadow the designer).
pub const RESERVED_TYPE_NEW: &str = "new";

/// Reserved first-positional token: `agentpit workflow list` prints the configured workflow
/// types, so a user-defined type may never be named `list` (it would be unreachable).
pub const RESERVED_TYPE_LIST: &str = "list";

/// The effective workflow after applying a named TYPE preset over the base `[workflow]`. All of
/// `run_capture`'s knobs read from here so a type override and the base path share one code path.
#[derive(Debug, Clone, PartialEq)]
struct EffectiveWorkflow {
    /// The TYPE's manager override (`[workflow.types.<name>].manager_backend`). Selecting a type
    /// is an explicit per-kind choice, so this outranks `[workflow.roles.manager]`'s backend
    /// (the role still contributes its persona + model either way).
    type_manager_backend: Option<BackendId>,
    /// The base `[workflow].manager_backend` fallback (below the manager role's backend).
    manager_backend: Option<BackendId>,
    max_depth: u32,
    max_calls_per_manager: u32,
    use_mcp: bool,
    enable_ask_human: bool,
    /// Which worker roles to dispatch to. `None` = every configured worker role (base behavior);
    /// `Some(names)` = the type's ordered subset of the shared cast.
    role_filter: Option<Vec<String>>,
    /// The type's manager BRIEF (`[workflow.types.<name>].prompt`), injected into the prompt.
    brief: Option<String>,
    /// The resolved type name, for the leader line. `None` = base workflow.
    type_name: Option<String>,
}

/// Resolve an optional workflow `type` name into an [`EffectiveWorkflow`] over `section`. `None`
/// returns the base `[workflow]` verbatim; a name looks up `[workflow.types.<name>]` and layers
/// its overrides (unset per-type fields inherit the base). An unknown name is a hard error that
/// lists the configured types. Pure and unit-tested.
fn resolve_workflow_type(
    section: &WorkflowSection,
    type_name: Option<&str>,
) -> Result<EffectiveWorkflow> {
    let base = EffectiveWorkflow {
        type_manager_backend: None,
        manager_backend: section.manager_backend,
        max_depth: section.max_depth,
        max_calls_per_manager: section.max_calls_per_manager,
        use_mcp: section.use_mcp,
        enable_ask_human: section.enable_ask_human,
        role_filter: None,
        brief: None,
        type_name: None,
    };
    let Some(name) = type_name else {
        return Ok(base);
    };
    let t = section.types.get(name).ok_or_else(|| {
        let configured = if section.types.is_empty() {
            "(none configured)".to_string()
        } else {
            section.types.keys().cloned().collect::<Vec<_>>().join(", ")
        };
        anyhow::anyhow!("unknown workflow type '{name}'. configured types: {configured}")
    })?;
    Ok(EffectiveWorkflow {
        type_manager_backend: t.manager_backend,
        manager_backend: base.manager_backend,
        max_depth: t.max_depth.unwrap_or(base.max_depth),
        max_calls_per_manager: t.max_calls_per_manager.unwrap_or(base.max_calls_per_manager),
        use_mcp: t.use_mcp.unwrap_or(base.use_mcp),
        enable_ask_human: t.enable_ask_human.unwrap_or(base.enable_ask_human),
        role_filter: (!t.roles.is_empty()).then(|| t.roles.clone()),
        brief: t.prompt.clone(),
        type_name: Some(name.to_string()),
    })
}

/// `agentpit workflow list` — print the base `[workflow]` and every configured
/// `[workflow.types.<name>]` preset with its EFFECTIVE values (type override → base), so the
/// listing shows exactly what `agentpit workflow <type> "<goal>"` would run. `--json` emits the
/// same summary machine-readable (for the dashboard and scripts).
pub async fn list(json: bool) -> Result<()> {
    let ctx = load_context()?;
    let config = &ctx.loaded.config;
    let path = crate::config::default_config_path();
    let listing = build_types_listing(&config.workflow, config.default.backend, &path);
    if json {
        println!("{}", serde_json::to_string_pretty(&listing)?);
    } else {
        print!("{}", render_types_listing(&listing));
    }
    Ok(())
}

/// One entry in the `workflow list` output: the base `[workflow]` or one named type, already
/// resolved to its effective values. Serialized verbatim for `--json`.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct TypeListingEntry {
    /// Type name; `None` = the base `[workflow]`.
    name: Option<String>,
    title: Option<String>,
    /// Copy-pasteable invocation.
    invoke: String,
    /// The manager that would actually run (explicit-arg step excluded), or a diagnostic when
    /// `[workflow.roles.manager]` is misconfigured.
    manager: String,
    /// Whether that manager can actually run a workflow (claude|codex). `false` means the run
    /// would abort at startup — the listing surfaces it instead of hiding it.
    manager_supported: bool,
    max_depth: u32,
    max_calls_per_manager: u32,
    use_mcp: bool,
    enable_ask_human: bool,
    /// Worker roles this entry dispatches to, in roster order. Empty = no roles configured
    /// (legacy flat-backend roster).
    roles: Vec<String>,
    /// Roles named by the type but missing from the shared `[workflow.roles.*]` cast (would be
    /// skipped with a warning at run time).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    roles_missing_from_cast: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    brief: Option<String>,
}

/// The full `workflow list` summary. Pure data — built by [`build_types_listing`], rendered by
/// [`render_types_listing`] or serialized for `--json`.
#[derive(Debug, Clone, PartialEq, Serialize)]
struct TypesListing {
    config_path: String,
    base: TypeListingEntry,
    types: Vec<TypeListingEntry>,
}

/// Build the listing from the config. Pure and unit-tested. Per-type resolution reuses
/// [`resolve_workflow_type`] so the listing can never drift from what a run would do; a
/// misconfigured `[workflow.roles.manager]` degrades to a diagnostic string instead of failing
/// the whole listing (`list` is a read-only inspection command).
fn build_types_listing(
    section: &WorkflowSection,
    default_backend: BackendId,
    config_path: &std::path::Path,
) -> TypesListing {
    let all_workers: Vec<String> = roles::worker_roles(&section.roles)
        .map(|(name, _)| name.clone())
        .collect();

    let entry = |name: Option<&str>| -> TypeListingEntry {
        // Both lookups are infallible here: `None` and keys taken from `section.types` always
        // resolve, so this expect can only trip on a future refactor.
        let eff = resolve_workflow_type(section, name)
            .expect("listing resolves only configured type names");
        let (manager, manager_supported) = match resolve_manager_backend(
            None,
            eff.type_manager_backend,
            &section.roles,
            eff.manager_backend,
            default_backend,
        ) {
            Ok((backend, _, _)) => (backend.to_string(), is_supported_manager(backend)),
            Err(_) => ("(invalid [workflow.roles.manager])".to_string(), false),
        };
        let (roles, missing) = match &eff.role_filter {
            // A type's ordered subset: split into cast members vs names that would be skipped.
            Some(filter) => filter
                .iter()
                .cloned()
                .partition(|n| all_workers.contains(n)),
            // Base behavior: every configured worker role.
            None => (all_workers.clone(), Vec::new()),
        };
        let (title, invoke) = match name {
            Some(n) => (
                section.types.get(n).and_then(|t| t.title.clone()),
                format!("agentpit workflow {n} \"<goal>\""),
            ),
            None => (None, "agentpit workflow \"<goal>\"".to_string()),
        };
        TypeListingEntry {
            name: name.map(str::to_string),
            title,
            invoke,
            manager,
            manager_supported,
            max_depth: eff.max_depth,
            max_calls_per_manager: eff.max_calls_per_manager,
            use_mcp: eff.use_mcp,
            enable_ask_human: eff.enable_ask_human,
            roles,
            roles_missing_from_cast: missing,
            brief: eff.brief,
        }
    };

    TypesListing {
        config_path: config_path.display().to_string(),
        base: entry(None),
        types: section.types.keys().map(|n| entry(Some(n))).collect(),
    }
}

/// Render the listing for the terminal. Pure and unit-tested.
fn render_types_listing(listing: &TypesListing) -> String {
    let mut out = String::new();
    out.push_str(&format!("config: {}\n\n", listing.config_path));

    let push_entry = |out: &mut String, e: &TypeListingEntry| {
        out.push_str(&format!("    run: {}\n", e.invoke));
        let manager = if e.manager_supported {
            e.manager.clone()
        } else {
            // A run with this manager aborts at startup — say so here instead of at run time.
            format!("{} (unsupported — workflow managers: claude, codex)", e.manager)
        };
        out.push_str(&format!(
            "    manager: {}   max_depth: {}   max_calls: {}   mcp: {}   ask_human: {}\n",
            manager,
            e.max_depth,
            e.max_calls_per_manager,
            on_off(e.use_mcp),
            on_off(e.enable_ask_human),
        ));
        let mut roles_line = if e.roles.is_empty() {
            "(none — flat backend roster)".to_string()
        } else {
            e.roles.join(", ")
        };
        for missing in &e.roles_missing_from_cast {
            roles_line.push_str(&format!(", {missing}? (not in cast)"));
        }
        out.push_str(&format!("    roles: {roles_line}\n"));
        if let Some(brief) = &e.brief {
            out.push_str(&format!("    brief: {}\n", truncate_line(brief, 100)));
        }
    };

    out.push_str("  base [workflow]\n");
    push_entry(&mut out, &listing.base);
    out.push('\n');

    if listing.types.is_empty() {
        out.push_str("  workflow types: (none configured)\n");
        out.push_str("    generate one:  agentpit workflow new \"<description>\"\n");
        return out;
    }

    out.push_str(&format!("  workflow types ({}):\n\n", listing.types.len()));
    for e in &listing.types {
        let name = e.name.as_deref().unwrap_or("?");
        match &e.title {
            Some(title) => out.push_str(&format!("  {name} — {title}\n")),
            None => out.push_str(&format!("  {name}\n")),
        }
        push_entry(&mut out, e);
        out.push('\n');
    }
    out
}

fn on_off(v: bool) -> &'static str {
    if v { "on" } else { "off" }
}

/// First line of `s`, truncated to `max` chars with an ellipsis (for one-line brief previews).
fn truncate_line(s: &str, max: usize) -> String {
    let line = s.lines().next().unwrap_or("").trim();
    if line.chars().count() <= max {
        line.to_string()
    } else {
        let cut: String = line.chars().take(max).collect();
        format!("{}…", cut.trim_end())
    }
}

/// Resolve the manager backend and its optional persona. Resolution order for the backend:
/// explicit CLI arg → the TYPE's `[workflow.types.<t>].manager_backend` (selecting a type is an
/// explicit per-kind choice) → `[workflow.roles.manager]` (first claude|codex in its list) →
/// `[workflow].manager_backend` → the default backend. The persona applies whenever the manager
/// role exists — a persona-only role (empty `backends`) still colors a legacy-resolved manager.
/// Pure and unit-tested; the `is_supported_manager` check on the final choice stays in
/// `run_capture` (an explicit arg may still name an unsupported backend).
fn resolve_manager_backend(
    explicit: Option<BackendId>,
    type_manager: Option<BackendId>,
    role_map: &BTreeMap<String, RoleConfig>,
    manager_backend: Option<BackendId>,
    default_backend: BackendId,
) -> Result<(BackendId, Option<String>, Option<String>)> {
    let manager_role = roles::resolve_manager(role_map, is_supported_manager)?;
    let backend = explicit
        .or(type_manager)
        .or(manager_role.as_ref().and_then(|m| m.backend))
        .or(manager_backend)
        .unwrap_or(default_backend);
    let persona = manager_role.as_ref().and_then(|m| m.prompt.clone());
    let model = manager_role.and_then(|m| m.model);
    Ok((backend, persona, model))
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
    filter: Option<&[String]>,
) -> Result<Option<RoleRoster>> {
    if roles::worker_roles(role_map).next().is_none() {
        return Ok(None);
    }
    let mut lines = Vec::new();
    let mut names = Vec::new();
    let mut skipped = Vec::new();

    // Roster order: a TYPE's ordered subset when a filter is given (names not in the shared cast,
    // or the reserved manager, are skipped with a reason), else every configured worker role.
    let selected: Vec<(&String, &RoleConfig)> = match filter {
        Some(want) => want
            .iter()
            .filter_map(|n| {
                if n == roles::MANAGER_ROLE {
                    skipped.push((n.clone(), "reserved manager role is not a dispatch target".into()));
                    None
                } else if let Some(role) = role_map.get(n) {
                    Some((n, role))
                } else {
                    skipped.push((n.clone(), "not defined in [workflow.roles.*]".into()));
                    None
                }
            })
            .collect(),
        None => roles::worker_roles(role_map).collect(),
    };

    for (name, role) in selected {
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
    /// The workflow BRIEF from `[workflow.types.<name>].prompt` (a selected named workflow).
    pub brief: Option<&'a str>,
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

/// The workflow-BRIEF block (from `[workflow.types.<name>].prompt`), or empty. Sits below the
/// manager persona so a selected type's high-level instructions lead the prompt body.
fn brief_block(brief: Option<&str>) -> String {
    match brief {
        None => String::new(),
        Some(b) => format!("WORKFLOW BRIEF:\n{b}\n\n", b = b.trim()),
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
    let brief = brief_block(roles.brief);
    format!(
        "=== AGENTPIT WORKFLOW ORCHESTRATOR ===\n\
You are the MANAGER agent for a multi-step coding workflow. Decompose the goal into\n\
sub-tasks, dispatch each to the best worker backend, read results, and write a final\n\
synthesis. Your LAST message MUST be the synthesis — do not stop after the last dispatch.\n\
\n\
{persona}\
{brief}\
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
    let brief = brief_block(roles.brief);
    format!(
        "=== AGENTPIT WORKFLOW ORCHESTRATOR (MCP MODE) ===\n\
You are the MANAGER agent for a multi-step coding workflow. Decompose the goal into\n\
sub-tasks, dispatch each to the best worker backend, read results, and write a final\n\
synthesis. Your LAST message MUST be the synthesis — do not stop after the last dispatch.\n\
\n\
{persona}\
{brief}\
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

// ── workflow designer (`agentpit workflow new "<description>"`) ───────────────────────────────
// One-shot generation: a backend turns a natural-language description into a STRUCTURED workflow
// proposal (a [workflow.types.<name>] preset + the roles it needs). Emitted as JSON (`--json`,
// consumed by the dashboard's ✨ generate button) or human-readable TOML (default), and optionally
// appended to config.toml (`--write`). The model output is DATA — parsed into `WorkflowProposal`,
// sanitized, and re-serialized; it is never executed.

/// One role in a generated proposal (mirrors `[workflow.roles.<name>]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProposalRole {
    pub name: String,
    #[serde(default)]
    pub backends: Vec<String>,
    #[serde(default)]
    pub prompt: Option<String>,
}

/// One illustrative blueprint step (the dashboard renders these on the canvas; the TOML output
/// ignores them — steps are a sketch, never persisted, per "cast not script").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProposalStep {
    pub name: String,
    #[serde(default)]
    pub manager: Option<String>,
    #[serde(default)]
    pub persona: Option<String>,
    #[serde(default)]
    pub behavior: Option<String>,
    #[serde(default)]
    pub workers: Vec<String>,
    #[serde(default)]
    pub dynamic: Option<bool>,
    #[serde(default)]
    pub ask: Option<bool>,
    #[serde(default)]
    pub fanout: Option<u32>,
}

/// A generated workflow proposal — the JSON contract shared verbatim with the dashboard.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkflowProposal {
    #[serde(rename = "type")]
    pub type_name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub manager_backend: Option<String>,
    #[serde(default)]
    pub brief: Option<String>,
    #[serde(default)]
    pub roles: Vec<ProposalRole>,
    #[serde(default)]
    pub uses_roles: Vec<String>,
    #[serde(default)]
    pub max_depth: Option<u32>,
    #[serde(default)]
    pub max_calls_per_manager: Option<u32>,
    #[serde(default)]
    pub use_mcp: Option<bool>,
    #[serde(default)]
    pub enable_ask_human: Option<bool>,
    #[serde(default)]
    pub steps: Vec<ProposalStep>,
}

/// `agentpit workflow new "<description>"` — the workflow designer. Launches a backend one-shot to
/// turn `description` into a [`WorkflowProposal`], then emits it as JSON (`--json`), human-readable
/// TOML (default), or appends it to config.toml (`--write`).
pub async fn generate(
    description: String,
    manager: Option<BackendId>,
    model: Option<String>,
    json: bool,
    write: bool,
    cwd: Option<String>,
) -> Result<()> {
    let cwd = resolve_cwd(cwd)?;
    let ctx = load_context()?;
    let config = &ctx.loaded.config;

    // Any capable backend can design; prefer the configured manager but do NOT require a
    // supported-manager (the designer only reads + emits JSON, no sub-dispatch). Normal dispatch
    // posture (like rescue), never the workflow's dangerous full-access posture.
    let backend = manager
        .or(config.workflow.manager_backend)
        .unwrap_or(config.default.backend);
    let auth = check_auth(backend).await;
    if !auth.ok {
        anyhow::bail!(
            "[{backend}] not authenticated. Run `{}`, or call `agentpit login {backend}`.",
            auth.login_command
        );
    }

    let available: Vec<BackendId> = ctx.regs.available().into_iter().collect();
    let prompt = designer_prompt(&description, &available);

    // Progress → STDERR so STDOUT carries ONLY the result (the dashboard parses stdout).
    eprintln!("Designing a workflow with {backend}… (one-shot; no sub-agents)");

    let cancel = CancellationToken::new();
    install_ctrlc_cancel(cancel.clone());
    let noop: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(|_: &str| {});
    let designer_model = roles::resolve_model(
        model.as_deref(),
        None,
        config.backends.get(&backend).and_then(|o| o.model.as_deref()),
    );
    let res = dispatch(backend, &prompt, &cwd, cancel, noop, &ctx.regs, designer_model.as_deref())
        .await?;
    if res.auth_failed {
        anyhow::bail!(
            "[{backend}] auth failure during generation. Run `{}`.",
            auth.login_command
        );
    }

    let proposal = parse_proposal(&res.output).map_err(|e| {
        let snippet: String = res.output.chars().take(800).collect();
        anyhow::anyhow!("{e}\n--- raw model output (first 800 chars) ---\n{snippet}")
    })?;

    if json {
        println!("{}", serde_json::to_string_pretty(&proposal)?);
        return Ok(());
    }

    let path = crate::config::default_config_path();
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let toml = render_proposal_toml(&proposal, &existing);
    if write {
        if existing.contains(&format!("[workflow.types.{}]", proposal.type_name)) {
            anyhow::bail!(
                "workflow type '{}' already exists in {}; choose another name or edit it in the dashboard.",
                proposal.type_name,
                path.display()
            );
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ok();
        }
        let mut updated = existing.clone();
        if !updated.is_empty() && !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push_str("\n# Generated by `agentpit workflow new`\n");
        updated.push_str(&toml);
        std::fs::write(&path, &updated)
            .map_err(|e| anyhow::anyhow!("failed to write {}: {e}", path.display()))?;
        eprintln!("Wrote workflow '{}' to {}", proposal.type_name, path.display());
    } else {
        println!("{toml}");
        eprintln!("\nReview the above, then append it to your config (or re-run with --write).");
    }
    eprintln!(
        "Run it with:  agentpit workflow {} \"<goal>\"",
        proposal.type_name
    );
    Ok(())
}

/// Build the designer system-prompt. Teaches the model the JSON schema and PINS the real backend
/// ids it may choose from, so `manager_backend`/`backends` never name a backend agentpit lacks.
fn designer_prompt(description: &str, available: &[BackendId]) -> String {
    let ids = available
        .iter()
        .map(BackendId::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "=== AGENTPIT WORKFLOW DESIGNER ===\n\
You design a REUSABLE agentpit workflow from a description. agentpit runs a model-driven\n\
workflow: a MANAGER backend decomposes a goal at runtime and dispatches sub-tasks to named\n\
ROLES (each role is a persona bound to a backend preference list). You are NOT solving any\n\
task — you DEFINE the cast and brief a manager will reuse for tasks of this kind.\n\
\n\
Output ONE JSON object and NOTHING else (no prose, no markdown fence). Schema:\n\
{{\n\
  \"type\": \"<kebab machine name, e.g. code-review; never 'new'>\",\n\
  \"title\": \"<short human label>\",\n\
  \"manager_backend\": \"<one of: {ids}>\",\n\
  \"brief\": \"<2-4 sentences telling the manager how to run THIS kind of workflow>\",\n\
  \"roles\": [ {{ \"name\": \"<kebab>\", \"backends\": [\"<id>\"], \"prompt\": \"<persona, 1-2 sentences>\" }} ],\n\
  \"uses_roles\": [\"<role name>\"],\n\
  \"max_depth\": 3, \"max_calls_per_manager\": 8, \"use_mcp\": false, \"enable_ask_human\": true,\n\
  \"steps\": [ {{ \"name\": \"<phase>\", \"manager\": \"<id>\", \"persona\": \"<why>\", \"behavior\": \"<directive>\", \"workers\": [\"<role>\"], \"dynamic\": true, \"ask\": false, \"fanout\": 3 }} ]\n\
}}\n\
\n\
RULES: every backend id MUST be one of [{ids}]. type + role names are lowercase [a-z0-9_-]\n\
starting alphanumeric. Keep the cast small (2-5 roles). Personas describe the ROLE, not the\n\
specific task. `steps` is an optional illustrative sketch. If the description is vague, design\n\
a sensible general-purpose workflow.\n\
\n\
=== DESCRIPTION ===\n\
{description}\n"
    )
}

/// Extract the first balanced JSON object from raw model output (tolerating a ```json fence or
/// leading/trailing prose). Returns the `{...}` slice.
fn extract_json(raw: &str) -> Result<&str> {
    let start = raw
        .find('{')
        .ok_or_else(|| anyhow::anyhow!("no JSON object in model output"))?;
    let bytes = raw.as_bytes();
    let mut depth = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for i in start..bytes.len() {
        let c = bytes[i];
        if in_str {
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_str = false;
            }
        } else {
            match c {
                b'"' => in_str = true,
                b'{' => depth += 1,
                b'}' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(&raw[start..=i]);
                    }
                }
                _ => {}
            }
        }
    }
    Err(anyhow::anyhow!("unterminated JSON object in model output"))
}

/// Parse + sanitize model output into a [`WorkflowProposal`].
fn parse_proposal(raw: &str) -> Result<WorkflowProposal> {
    let json = extract_json(raw)?;
    let mut p: WorkflowProposal = serde_json::from_str(json)
        .map_err(|e| anyhow::anyhow!("could not parse the generated workflow JSON: {e}"))?;
    normalize_proposal(&mut p)?;
    Ok(p)
}

/// Sanitize a proposal so it can never inject bad config: names → lowercase-kebab, backends
/// filtered to known ids, `uses_roles` restricted to defined role names.
fn normalize_proposal(p: &mut WorkflowProposal) -> Result<()> {
    let known: Vec<String> = BackendId::ALL.iter().map(|b| b.as_str().to_string()).collect();
    p.type_name = sanitize_name(&p.type_name);
    if p.type_name.is_empty()
        || p.type_name == RESERVED_TYPE_NEW
        || p.type_name == RESERVED_TYPE_LIST
    {
        anyhow::bail!("the model generated an invalid workflow type name");
    }
    for r in &mut p.roles {
        r.name = sanitize_name(&r.name);
        r.backends.retain(|b| known.contains(b));
        if r.prompt.as_deref().is_some_and(|pr| pr.trim().is_empty()) {
            r.prompt = None;
        }
    }
    p.roles.retain(|r| !r.name.is_empty());
    let defined: Vec<String> = p.roles.iter().map(|r| r.name.clone()).collect();
    p.uses_roles = p
        .uses_roles
        .iter()
        .map(|n| sanitize_name(n))
        .filter(|n| defined.contains(n))
        .collect();
    if p
        .manager_backend
        .as_ref()
        .is_some_and(|mb| !known.contains(mb))
    {
        p.manager_backend = None;
    }
    Ok(())
}

/// Lowercase, keep `[a-z0-9_-]`, map spaces to `-`, and drop leading `-`/`_` (names must start
/// alphanumeric — mirrors the role-name rule enforced in the dashboard + `settings.rs`).
fn sanitize_name(s: &str) -> String {
    let lowered = s.trim().to_lowercase();
    let mut out = String::new();
    for c in lowered.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' {
            out.push(c);
        } else if c == ' ' {
            out.push('-');
        }
    }
    out.trim_start_matches(['-', '_']).to_string()
}

/// Render the proposal as the TOML a user would paste into config.toml: new `[workflow.roles.*]`
/// (skipping ones already present by name in `existing`) + the `[workflow.types.<name>]` table.
fn render_proposal_toml(p: &WorkflowProposal, existing: &str) -> String {
    let mut out = String::new();
    for r in &p.roles {
        if existing.contains(&format!("[workflow.roles.{}]", r.name)) {
            continue;
        }
        out.push_str(&format!("\n[workflow.roles.{}]\n", r.name));
        out.push_str(&format!("backends = [{}]\n", toml_str_array(&r.backends)));
        if let Some(pr) = &r.prompt {
            out.push_str(&format!("prompt = {}\n", toml_string(pr)));
        }
    }
    out.push_str(&format!("\n[workflow.types.{}]\n", p.type_name));
    if let Some(t) = &p.title {
        out.push_str(&format!("title = {}\n", toml_string(t)));
    }
    if let Some(b) = &p.brief {
        out.push_str(&format!("prompt = {}\n", toml_string(b)));
    }
    if !p.uses_roles.is_empty() {
        out.push_str(&format!("roles = [{}]\n", toml_str_array(&p.uses_roles)));
    }
    if let Some(mb) = &p.manager_backend {
        out.push_str(&format!("manager_backend = \"{mb}\"\n"));
    }
    if let Some(v) = p.max_depth {
        out.push_str(&format!("max_depth = {v}\n"));
    }
    if let Some(v) = p.max_calls_per_manager {
        out.push_str(&format!("max_calls_per_manager = {v}\n"));
    }
    if let Some(v) = p.use_mcp {
        out.push_str(&format!("use_mcp = {v}\n"));
    }
    if let Some(v) = p.enable_ask_human {
        out.push_str(&format!("enable_ask_human = {v}\n"));
    }
    out
}

/// Render a slice of strings as a TOML inline-array body (`"a", "b"`), quoting each. The items
/// here (backend ids, role names) are already validated to a bare-key charset, so no escaping.
fn toml_str_array(items: &[String]) -> String {
    items
        .iter()
        .map(|x| format!("\"{x}\""))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Minimal TOML basic-string escaping for a value we emit (quotes, backslashes, control chars).
fn toml_string(s: &str) -> String {
    let esc = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\t', "\\t");
    format!("\"{esc}\"")
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
            brief: None,
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
            brief: None,
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
                        model: None,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn roster_is_none_without_worker_roles() {
        assert_eq!(build_role_roster(&BTreeMap::new(), &[], None).unwrap(), None);
        // A manager-only config is still legacy mode for the roster.
        let manager_only = role_map(&[("manager", &[BackendId::Claude], None)]);
        assert_eq!(
            build_role_roster(&manager_only, &[BackendId::Claude], None).unwrap(),
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
        let roster = build_role_roster(&roles, &[BackendId::Claude], None)
            .unwrap()
            .unwrap();
        assert_eq!(roster.names, vec!["implementer"]);
        assert!(roster.lines.contains("implementer (claude): You implement."));
        assert_eq!(roster.skipped.len(), 1);
        assert_eq!(roster.skipped[0].0, "reviewer");
        assert!(roster.skipped[0].1.contains("codex"));
    }

    #[test]
    fn roster_errors_when_every_worker_role_is_unresolvable() {
        let roles = role_map(&[("reviewer", &[BackendId::Codex], None)]);
        let err = build_role_roster(&roles, &[], None).unwrap_err().to_string();
        assert!(err.contains("no configured worker role could be resolved"));
        assert!(err.contains("reviewer"));
    }

    #[test]
    fn manager_resolution_order_explicit_then_type_then_role_then_config_then_default() {
        let roles = role_map(&[(
            "manager",
            &[BackendId::Codex],
            Some("plan tightly"),
        )]);
        // Explicit CLI arg wins over everything; the persona still applies.
        let (backend, persona, _) = resolve_manager_backend(
            Some(BackendId::Claude),
            Some(BackendId::Codex),
            &roles,
            Some(BackendId::Claude),
            BackendId::Antigravity,
        )
        .unwrap();
        assert_eq!(backend, BackendId::Claude);
        assert_eq!(persona.as_deref(), Some("plan tightly"));
        // The TYPE's manager_backend wins over the role (selecting a type is an explicit
        // per-kind choice); the role persona still applies.
        let (backend, persona, _) = resolve_manager_backend(
            None,
            Some(BackendId::Claude),
            &roles,
            None,
            BackendId::Antigravity,
        )
        .unwrap();
        assert_eq!(backend, BackendId::Claude);
        assert_eq!(persona.as_deref(), Some("plan tightly"));
        // The role wins over [workflow].manager_backend.
        let (backend, _, _) = resolve_manager_backend(
            None,
            None,
            &roles,
            Some(BackendId::Claude),
            BackendId::Antigravity,
        )
        .unwrap();
        assert_eq!(backend, BackendId::Codex);
        // No role → config → default.
        let empty = BTreeMap::new();
        let (backend, persona, _) = resolve_manager_backend(
            None,
            None,
            &empty,
            Some(BackendId::Claude),
            BackendId::Antigravity,
        )
        .unwrap();
        assert_eq!(backend, BackendId::Claude);
        assert_eq!(persona, None);
        let (backend, _, _) =
            resolve_manager_backend(None, None, &empty, None, BackendId::Antigravity).unwrap();
        assert_eq!(backend, BackendId::Antigravity);
    }

    #[test]
    fn persona_only_manager_role_keeps_legacy_backend_but_carries_persona() {
        let roles = role_map(&[("manager", &[], Some("persona only"))]);
        let (backend, persona, _) = resolve_manager_backend(
            None,
            None,
            &roles,
            Some(BackendId::Codex),
            BackendId::Antigravity,
        )
        .unwrap();
        assert_eq!(backend, BackendId::Codex);
        assert_eq!(persona.as_deref(), Some("persona only"));
    }

    #[test]
    fn manager_role_with_unsupported_backends_propagates_the_error() {
        // Gemini cannot manage (is_supported_manager: claude|codex only).
        let roles = role_map(&[("manager", &[BackendId::Gemini], None)]);
        assert!(
            resolve_manager_backend(None, None, &roles, None, BackendId::Antigravity).is_err()
        );
    }

    // ---- named workflow types ----

    fn section_with_types() -> WorkflowSection {
        let mut s = WorkflowSection {
            manager_backend: Some(BackendId::Codex),
            ..WorkflowSection::default()
        };
        s.roles = role_map(&[
            ("reviewer", &[BackendId::Codex], Some("review")),
            ("security", &[BackendId::Claude], Some("sec")),
            ("impl", &[BackendId::Claude], Some("build")),
        ]);
        s.types.insert(
            "review".into(),
            crate::config::WorkflowType {
                title: Some("Strict review".into()),
                prompt: Some("Run a strict review.".into()),
                roles: vec!["reviewer".into(), "security".into()],
                manager_backend: Some(BackendId::Claude),
                max_depth: Some(2),
                max_calls_per_manager: None,
                use_mcp: None,
                enable_ask_human: Some(true),
            },
        );
        s
    }

    #[test]
    fn resolve_type_none_returns_base_workflow() {
        let s = section_with_types();
        let eff = resolve_workflow_type(&s, None).unwrap();
        assert_eq!(eff.type_manager_backend, None);
        assert_eq!(eff.manager_backend, Some(BackendId::Codex));
        assert_eq!(eff.max_depth, 3);
        assert!(eff.role_filter.is_none());
        assert!(eff.brief.is_none());
        assert!(eff.type_name.is_none());
    }

    #[test]
    fn resolve_type_layers_overrides_over_base() {
        let s = section_with_types();
        let eff = resolve_workflow_type(&s, Some("review")).unwrap();
        assert_eq!(eff.type_manager_backend, Some(BackendId::Claude)); // type override
        assert_eq!(eff.manager_backend, Some(BackendId::Codex)); // base fallback kept separate
        assert_eq!(eff.max_depth, 2); // type override
        assert_eq!(eff.max_calls_per_manager, 8); // inherited base default
        assert!(eff.enable_ask_human); // type override
        assert_eq!(
            eff.role_filter.as_deref(),
            Some(["reviewer".to_string(), "security".to_string()].as_slice())
        );
        assert_eq!(eff.brief.as_deref(), Some("Run a strict review."));
        assert_eq!(eff.type_name.as_deref(), Some("review"));
    }

    #[test]
    fn resolve_unknown_type_errors_and_lists_configured() {
        let s = section_with_types();
        let err = resolve_workflow_type(&s, Some("nope"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown workflow type 'nope'"), "got: {err}");
        assert!(err.contains("review"), "should list configured types: {err}");
    }

    // ---- workflow list ----

    #[test]
    fn listing_resolves_effective_values_per_type() {
        let s = section_with_types();
        let listing =
            build_types_listing(&s, BackendId::Antigravity, std::path::Path::new("/tmp/c.toml"));

        // Base: [workflow].manager_backend, defaults, and every worker role from the cast.
        assert_eq!(listing.base.name, None);
        assert_eq!(listing.base.manager, "codex");
        assert!(listing.base.manager_supported);
        assert_eq!(listing.base.max_depth, 3);
        assert_eq!(listing.base.invoke, "agentpit workflow \"<goal>\"");
        assert_eq!(listing.base.roles, vec!["impl", "reviewer", "security"]);
        assert!(listing.base.brief.is_none());

        // Type: overrides layered over the base, roster narrowed to the filter in order.
        assert_eq!(listing.types.len(), 1);
        let t = &listing.types[0];
        assert_eq!(t.name.as_deref(), Some("review"));
        assert_eq!(t.title.as_deref(), Some("Strict review"));
        assert_eq!(t.invoke, "agentpit workflow review \"<goal>\"");
        assert_eq!(t.manager, "claude"); // type override
        assert_eq!(t.max_depth, 2); // type override
        assert_eq!(t.max_calls_per_manager, 8); // inherited base default
        assert!(t.enable_ask_human);
        assert_eq!(t.roles, vec!["reviewer", "security"]);
        assert!(t.roles_missing_from_cast.is_empty());
        assert_eq!(t.brief.as_deref(), Some("Run a strict review."));
    }

    #[test]
    fn listing_render_shows_types_and_invocations() {
        let s = section_with_types();
        let listing =
            build_types_listing(&s, BackendId::Antigravity, std::path::Path::new("/tmp/c.toml"));
        let text = render_types_listing(&listing);
        assert!(text.contains("config: /tmp/c.toml"), "got: {text}");
        assert!(text.contains("base [workflow]"), "got: {text}");
        assert!(text.contains("review — Strict review"), "got: {text}");
        assert!(
            text.contains("run: agentpit workflow review \"<goal>\""),
            "got: {text}"
        );
        assert!(text.contains("brief: Run a strict review."), "got: {text}");
        assert!(text.contains("ask_human: on"), "got: {text}");
    }

    #[test]
    fn listing_no_types_hints_the_generator() {
        let s = WorkflowSection::default();
        let listing =
            build_types_listing(&s, BackendId::Claude, std::path::Path::new("/tmp/c.toml"));
        assert!(listing.types.is_empty());
        assert!(listing.base.roles.is_empty()); // no cast → flat roster
        let text = render_types_listing(&listing);
        assert!(text.contains("(none configured)"), "got: {text}");
        assert!(
            text.contains("agentpit workflow new \"<description>\""),
            "got: {text}"
        );
        assert!(
            text.contains("(none — flat backend roster)"),
            "got: {text}"
        );
    }

    #[test]
    fn listing_flags_roles_missing_from_cast() {
        let mut s = section_with_types();
        s.types.get_mut("review").unwrap().roles =
            vec!["reviewer".into(), "ghost".into()];
        let listing =
            build_types_listing(&s, BackendId::Antigravity, std::path::Path::new("/tmp/c.toml"));
        let t = &listing.types[0];
        assert_eq!(t.roles, vec!["reviewer"]);
        assert_eq!(t.roles_missing_from_cast, vec!["ghost"]);
        let text = render_types_listing(&listing);
        assert!(text.contains("ghost? (not in cast)"), "got: {text}");
    }

    #[test]
    fn listing_survives_invalid_manager_role() {
        // Gemini-only manager role is a hard error at run time; the listing degrades instead.
        let mut s = section_with_types();
        s.roles = role_map(&[
            ("manager", &[BackendId::Gemini], None),
            ("reviewer", &[BackendId::Codex], Some("review")),
        ]);
        let listing =
            build_types_listing(&s, BackendId::Antigravity, std::path::Path::new("/tmp/c.toml"));
        assert_eq!(listing.base.manager, "(invalid [workflow.roles.manager])");
        assert!(!listing.base.manager_supported);
    }

    #[test]
    fn listing_type_manager_override_beats_manager_role() {
        // A manager role pins claude for the BASE, but the review type says codex — the type
        // wins for its own runs (regression: the role used to shadow the type's override).
        let mut s = section_with_types();
        s.roles.insert(
            "manager".into(),
            RoleConfig {
                backends: vec![BackendId::Claude],
                prompt: Some("coordinate".into()),
                model: None,
            },
        );
        s.types.get_mut("review").unwrap().manager_backend = Some(BackendId::Codex);
        let listing =
            build_types_listing(&s, BackendId::Antigravity, std::path::Path::new("/tmp/c.toml"));
        assert_eq!(listing.base.manager, "claude"); // role governs the base
        assert_eq!(listing.types[0].manager, "codex"); // type override governs the type
    }

    #[test]
    fn listing_marks_unsupported_manager_backends() {
        // No manager role, no [workflow].manager_backend → the default backend wins, but
        // antigravity cannot manage a workflow; the listing must say so, not hide it.
        let s = WorkflowSection::default();
        let listing =
            build_types_listing(&s, BackendId::Antigravity, std::path::Path::new("/tmp/c.toml"));
        assert_eq!(listing.base.manager, "antigravity");
        assert!(!listing.base.manager_supported);
        let text = render_types_listing(&listing);
        assert!(
            text.contains("antigravity (unsupported — workflow managers: claude, codex)"),
            "got: {text}"
        );
    }

    #[test]
    fn listing_json_uses_stable_keys() {
        let s = section_with_types();
        let listing =
            build_types_listing(&s, BackendId::Antigravity, std::path::Path::new("/tmp/c.toml"));
        let v = serde_json::to_value(&listing).unwrap();
        assert_eq!(v["base"]["manager"], "codex");
        assert_eq!(v["types"][0]["name"], "review");
        assert_eq!(v["types"][0]["max_calls_per_manager"], 8);
        assert_eq!(v["types"][0]["roles"][0], "reviewer");
        // Empty diagnostics are omitted from the JSON.
        assert!(v["types"][0].get("roles_missing_from_cast").is_none());
    }

    #[test]
    fn truncate_line_takes_first_line_and_caps_length() {
        assert_eq!(truncate_line("short brief", 100), "short brief");
        assert_eq!(truncate_line("line one\nline two", 100), "line one");
        let long = "x".repeat(120);
        let cut = truncate_line(&long, 100);
        assert_eq!(cut.chars().count(), 101); // 100 chars + ellipsis
        assert!(cut.ends_with('…'));
    }

    #[test]
    fn normalize_rejects_reserved_type_names() {
        for reserved in [RESERVED_TYPE_NEW, RESERVED_TYPE_LIST] {
            let mut p = WorkflowProposal {
                type_name: reserved.to_string(),
                ..Default::default()
            };
            let err = normalize_proposal(&mut p).unwrap_err().to_string();
            assert!(err.contains("invalid workflow type name"), "got: {err}");
        }
    }

    #[test]
    fn roster_filter_narrows_to_type_roles_in_order() {
        let roles = role_map(&[
            ("reviewer", &[BackendId::Codex], Some("r")),
            ("security", &[BackendId::Codex], Some("s")),
            ("impl", &[BackendId::Codex], Some("i")),
        ]);
        let available = [BackendId::Codex];
        let filter = vec!["security".to_string(), "reviewer".to_string()];
        let roster = build_role_roster(&roles, &available, Some(&filter))
            .unwrap()
            .unwrap();
        assert_eq!(roster.names, vec!["security", "reviewer"]); // filter order, impl excluded
        assert!(!roster.lines.contains("impl"));
    }

    #[test]
    fn roster_filter_skips_unknown_and_manager_names() {
        let roles = role_map(&[
            ("manager", &[BackendId::Claude], None),
            ("reviewer", &[BackendId::Codex], Some("r")),
        ]);
        let available = [BackendId::Codex, BackendId::Claude];
        let filter = vec![
            "reviewer".to_string(),
            "ghost".to_string(),
            "manager".to_string(),
        ];
        let roster = build_role_roster(&roles, &available, Some(&filter))
            .unwrap()
            .unwrap();
        assert_eq!(roster.names, vec!["reviewer"]);
        let skipped: Vec<&str> = roster.skipped.iter().map(|(n, _)| n.as_str()).collect();
        assert!(skipped.contains(&"ghost"));
        assert!(skipped.contains(&"manager"));
    }

    // ---- workflow designer ----

    #[test]
    fn designer_prompt_pins_available_backend_ids() {
        let p = designer_prompt("build a review flow", &[BackendId::Claude, BackendId::Codex]);
        assert!(p.contains("claude, codex"));
        assert!(p.contains("build a review flow"));
        assert!(p.contains("\"type\""));
    }

    #[test]
    fn extract_json_finds_object_amid_prose_and_fences() {
        let raw = "Sure!\n```json\n{\"type\": \"x\", \"roles\": []}\n```\nDone.";
        assert_eq!(extract_json(raw).unwrap(), "{\"type\": \"x\", \"roles\": []}");
        // Braces inside strings don't confuse the matcher.
        let raw2 = "{\"brief\": \"use {curly} braces\"}";
        assert_eq!(extract_json(raw2).unwrap(), raw2);
    }

    #[test]
    fn parse_proposal_sanitizes_names_and_backends() {
        let raw = r#"prose {"type":"Code Review!","title":"CR","manager_backend":"claude",
          "brief":"review","roles":[{"name":"Rev Iewer","backends":["codex","imaginary"],"prompt":"x"},
          {"name":"sec","backends":["claude"],"prompt":"  "}],
          "uses_roles":["rev-iewer","sec","ghost"],"enable_ask_human":true} trailing"#;
        let p = parse_proposal(raw).unwrap();
        assert_eq!(p.type_name, "code-review"); // sanitized (space→-, '!' dropped)
        assert_eq!(p.roles[0].name, "rev-iewer");
        assert_eq!(p.roles[0].backends, vec!["codex".to_string()]); // imaginary dropped
        assert_eq!(p.roles[1].prompt, None); // blank persona → None
        assert_eq!(p.uses_roles, vec!["rev-iewer".to_string(), "sec".to_string()]); // ghost dropped
        assert_eq!(p.manager_backend.as_deref(), Some("claude"));
    }

    #[test]
    fn parse_proposal_rejects_reserved_type_new() {
        assert!(parse_proposal(r#"{"type":"new","roles":[]}"#).is_err());
    }

    #[test]
    fn render_toml_emits_type_and_new_roles_only_and_reparses() {
        let p = WorkflowProposal {
            type_name: "review".into(),
            title: Some("Strict".into()),
            manager_backend: Some("claude".into()),
            brief: Some("Run a review.".into()),
            roles: vec![
                ProposalRole {
                    name: "reviewer".into(),
                    backends: vec!["codex".into()],
                    prompt: Some("crit".into()),
                },
                ProposalRole {
                    name: "security".into(),
                    backends: vec!["claude".into()],
                    prompt: None,
                },
            ],
            uses_roles: vec!["reviewer".into(), "security".into()],
            max_depth: Some(2),
            max_calls_per_manager: None,
            use_mcp: None,
            enable_ask_human: Some(true),
            steps: vec![],
        };
        let existing = "[workflow.roles.reviewer]\nbackends = [\"codex\"]\n";
        let toml = render_proposal_toml(&p, existing);
        // 'reviewer' already present → skipped; only 'security' + the type table are emitted.
        assert!(!toml.contains("[workflow.roles.reviewer]"));
        assert!(toml.contains("[workflow.roles.security]"));
        assert!(toml.contains("[workflow.types.review]"));
        assert!(toml.contains("roles = [\"reviewer\", \"security\"]"));
        assert!(toml.contains("enable_ask_human = true"));
        assert!(toml.contains("prompt = \"Run a review.\""));
        // The full config (existing + generated) must parse back into HubConfig.
        let full = format!("{existing}{toml}");
        let cfg: crate::config::HubConfig = toml::from_str(&full).expect("rendered TOML parses");
        assert!(cfg.workflow.types.contains_key("review"));
        assert_eq!(cfg.workflow.types["review"].roles.len(), 2);
    }
}
