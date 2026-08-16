//! `agentpit diagnose "<task>" [--json]` — a dry-run observation point (design §1.5).
//!
//! Shows the full diagnosis chain: extracted features → diagnosed `(category, confidence)`
//! → the backend a `rescue` dispatch would route the task to, and why. `--json` emits a
//! machine-readable verdict for downstream automation (the Phase B issue→routing GitHub
//! Action).
//!
//! The backend selection is the real thing: it calls `router::Router::resolve` for the
//! `rescue` route key, so every routing stage — the `[routes]` table, the similarity stage
//! (in `--features similarity` builds), the profile pick with its cost tiebreak,
//! long-context and keyword heuristics, and the default fallback — is reproduced instead
//! of mirrored.
//!
//! Scope: this is the *router's* answer, which is not always the whole dispatch. A bare
//! `agentpit rescue` with `[ensemble] rescue_members` configured fans out to those members
//! without consulting the router at all, and `--role` / `--cascade` likewise pick their
//! backend before routing. Those selections are dispatch-plan decisions layered above the
//! router; `diagnose` reports the routing stage only.

use std::collections::HashSet;

use anyhow::Result;
use console::style;
use serde::Serialize;

use crate::config::{HubConfig, RouteKey};
use crate::diagnose::{self, DiagnoseMethod, Diagnosis, LLM_ASSIST_CONFIDENCE_THRESHOLD};
use crate::profile::{ProfileSet, TaskCategory, load_profiles};
use crate::router::{RouteReason, RouteRequest, Router};
use crate::types::BackendId;

use super::load_context;

/// The routing verdict for a diagnosed task — a projection of the router's `RouteDecision`.
#[derive(Debug, Clone, Serialize)]
struct Routing {
    /// The backend the task would be sent to.
    backend: BackendId,
    /// The router's own reason string (`route_table`, `similarity`, `profile`,
    /// `profile_cost_tiebreak`, `auto_long_context`, `auto_keyword`, `default`).
    reason: String,
    /// True when a capability-profile stage picked `backend` (reason `profile*`), whether it
    /// scored the diagnosed category or the category-independent overall mean.
    from_profile: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    category: Option<TaskCategory>,
    #[serde(skip_serializing_if = "Option::is_none")]
    score: Option<u8>,
}

/// The full machine-readable diagnose report (`--json` output).
#[derive(Debug, Clone, Serialize)]
struct DiagnoseReport {
    task: String,
    diagnosis: Diagnosis,
    routing: Routing,
    available: Vec<BackendId>,
    /// Backends the auto-route stages are currently skipping (recent durable dispatch
    /// failure — quota / tier / auth). Explicit picks and `[routes]` pins still reach them.
    suspended: Vec<BackendId>,
    threshold: f32,
}

pub async fn run(task: String, json: bool) -> Result<()> {
    let ctx = load_context()?;
    let available = ctx.regs.available();
    let profiles = load_profiles(None)?;
    let suspended = crate::availability::recently_suspended();

    let report = build_report(&task, &ctx.loaded.config, &available, &suspended, profiles);

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_human(&report));
    }
    Ok(())
}

/// Build the report by running the real router. Pure aside from the diagnose heuristic:
/// `Router::resolve` is a pure function of `(config, available, profiles, task)`.
fn build_report(
    task: &str,
    config: &HubConfig,
    available: &HashSet<BackendId>,
    suspended: &HashSet<BackendId>,
    profiles: ProfileSet,
) -> DiagnoseReport {
    let diagnosis = diagnose::diagnose(task);

    // The router's own answer for the `rescue` route key, with no explicit backend — the
    // same call `rescue` makes once it has decided to route (see the module docs for the
    // dispatch-plan layers that can pre-empt routing entirely).
    let router =
        Router::new(config.clone(), available.clone(), profiles).with_suspended(suspended.clone());
    let decision = router.resolve(&RouteRequest {
        tool: RouteKey::Rescue,
        explicit_backend: None,
        task: Some(task),
    });
    let (category, score) = match decision.reason {
        RouteReason::Profile {
            category, score, ..
        } => (Some(category), Some(score)),
        // The overall stage routes on capability without a category: reporting the diagnosis's
        // own low-confidence guess here would read as "routed as Coding", which is the one
        // thing this stage deliberately does NOT do.
        RouteReason::ProfileOverall { score, .. } => (None, Some(score)),
        _ => (None, None),
    };
    let routing = Routing {
        backend: decision.backend,
        reason: decision.reason.as_str().to_string(),
        from_profile: matches!(
            decision.reason,
            RouteReason::Profile { .. } | RouteReason::ProfileOverall { .. }
        ),
        category,
        score,
    };

    let mut available_sorted: Vec<BackendId> = available.iter().copied().collect();
    available_sorted.sort();
    let mut suspended_sorted: Vec<BackendId> = suspended.iter().copied().collect();
    suspended_sorted.sort();

    DiagnoseReport {
        task: task.to_string(),
        diagnosis,
        routing,
        available: available_sorted,
        suspended: suspended_sorted,
        threshold: LLM_ASSIST_CONFIDENCE_THRESHOLD,
    }
}

fn method_str(method: DiagnoseMethod) -> &'static str {
    match method {
        DiagnoseMethod::Heuristic => "heuristic",
        DiagnoseMethod::LlmAssisted => "llm_assisted",
        DiagnoseMethod::Declared => "declared",
    }
}

/// Render the human-readable summary. Pure: builds and returns a fresh `String`.
fn render_human(report: &DiagnoseReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let d = &report.diagnosis;
    let f = &d.features;

    let _ = writeln!(out, "{} {}", style("diagnose:").bold(), report.task);

    let _ = writeln!(out, "\nfeatures:");
    let _ = writeln!(out, "  tokens       {}", f.token_estimate);
    let _ = writeln!(
        out,
        "  code_block   {}",
        if f.has_code_block { "yes" } else { "no" }
    );
    let verbs = if f.verbs.is_empty() {
        "(none)".to_string()
    } else {
        f.verbs.join(", ")
    };
    let _ = writeln!(out, "  verbs        {verbs}");
    let keywords = if f.matched_keywords.is_empty() {
        "(none)".to_string()
    } else {
        f.matched_keywords
            .iter()
            .map(|(c, kw)| format!("{}:{}", c.as_str(), kw))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let _ = writeln!(out, "  keywords     {keywords}");

    let _ = writeln!(
        out,
        "\ncategory: {} (confidence {:.2}, method {})",
        style(d.primary.as_str()).cyan(),
        d.confidence,
        method_str(d.method),
    );

    let r = &report.routing;
    let detail = match r.reason.as_str() {
        "profile" | "profile_cost_tiebreak" => format!(
            "{}: {} score {}",
            r.reason,
            r.category.map(|c| c.as_str()).unwrap_or("?"),
            r.score.unwrap_or(0)
        ),
        "route_table" => "route table: [routes] pins this tool before any auto-routing".into(),
        "default" if d.confidence < report.threshold => format!(
            "default — confidence {:.2} < {:.2}, diagnosis too weak for profile routing",
            d.confidence, report.threshold
        ),
        reason => reason.to_string(),
    };
    let _ = writeln!(out, "\nrouting:");
    let _ = writeln!(
        out,
        "  selected   {}   ({detail})",
        style(r.backend).green()
    );

    let avail = if report.available.is_empty() {
        "(none)".to_string()
    } else {
        report
            .available
            .iter()
            .map(|b| b.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let _ = writeln!(out, "  available  {avail}");
    if !report.suspended.is_empty() {
        let list = report
            .suspended
            .iter()
            .map(|b| b.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            out,
            "  suspended  {list}   (recent quota/auth failure — auto-route skips them; \
             --backend and [routes] pins still work)"
        );
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::seeded_profiles;

    fn all_seeded_available() -> HashSet<BackendId> {
        [
            BackendId::Claude,
            BackendId::Codex,
            BackendId::Antigravity,
            BackendId::Opencode,
            BackendId::Opencode,
        ]
        .into_iter()
        .collect()
    }

    /// Auto-route on, no `[routes]` pins — the profile stage decides.
    fn auto_route_config(default_backend: BackendId) -> HubConfig {
        let mut config = HubConfig::default();
        config.default.backend = default_backend;
        config
    }

    #[test]
    fn confident_coding_task_routes_to_profile_argmax() {
        let report = build_report(
            "implement a function to parse the duration feature",
            &auto_route_config(BackendId::Opencode),
            &all_seeded_available(),
            &HashSet::new(),
            seeded_profiles(),
        );

        assert_eq!(report.diagnosis.primary, TaskCategory::Coding);
        assert!(report.diagnosis.confidence >= LLM_ASSIST_CONFIDENCE_THRESHOLD);
        assert_eq!(report.routing.reason, "profile");
        assert!(report.routing.from_profile);
        // Claude is the seeded coding argmax.
        assert_eq!(report.routing.backend, BackendId::Claude);
        assert_eq!(report.routing.score, Some(88));
        assert_eq!(report.routing.category, Some(TaskCategory::Coding));
    }

    /// Strata review 2026-07-29: every other test passes an empty suspension set, which left
    /// the entire user-visible `suspended` surface — the human line, the JSON field, and the
    /// routing detour itself — unexecuted by the suite.
    #[test]
    fn a_suspended_backend_is_reported_and_routed_around() {
        let suspended: HashSet<BackendId> = [BackendId::Antigravity].into_iter().collect();
        // Antigravity is the seeded Docs argmax (86): with it suspended the profile stage
        // must fall to the next best, exactly as dispatch would.
        let report = build_report(
            "summarize the docs for the payment api documentation",
            &auto_route_config(BackendId::Opencode),
            &all_seeded_available(),
            &suspended,
            seeded_profiles(),
        );

        assert_eq!(report.suspended, vec![BackendId::Antigravity]);
        assert_eq!(report.routing.backend, BackendId::Claude);
        assert_eq!(report.routing.category, Some(TaskCategory::Docs));

        let human = render_human(&report);
        assert!(
            human.contains("suspended  antigravity"),
            "human render must name the suspended backend, got:\n{human}"
        );

        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains(r#""suspended":["antigravity"]"#));
    }

    /// Eval finding 6 (2026-07): diagnose used to mirror only the profile stage, so its
    /// printed route could differ from the deployed router. It now reproduces every stage —
    /// a `[routes] rescue` pin must win exactly like it does at dispatch time.
    #[test]
    fn route_table_pin_wins_exactly_like_the_deployed_router() {
        let mut config = auto_route_config(BackendId::Opencode);
        config
            .routes
            .insert(RouteKey::Rescue, BackendId::Antigravity);
        let report = build_report(
            "implement a function to parse the duration feature",
            &config,
            &all_seeded_available(),
            &HashSet::new(),
            seeded_profiles(),
        );

        assert_eq!(report.routing.reason, "route_table");
        assert_eq!(report.routing.backend, BackendId::Antigravity);
        assert!(!report.routing.from_profile);
    }

    #[test]
    fn low_confidence_task_routes_on_the_overall_score_without_a_category() {
        // Below the gate the diagnosis's category is not trustworthy, so the report must not
        // claim one — but the route is still a measured choice, not `default.backend`.
        let report = build_report(
            "alpha beta gamma",
            &auto_route_config(BackendId::Opencode),
            &all_seeded_available(),
            &HashSet::new(),
            seeded_profiles(),
        );

        assert!(report.diagnosis.confidence < LLM_ASSIST_CONFIDENCE_THRESHOLD);
        assert_eq!(report.routing.reason, "profile_overall");
        assert!(report.routing.from_profile);
        assert_eq!(report.routing.category, None, "no category was trusted");
        assert!(report.routing.score.is_some(), "the mean is a real reading");
    }

    #[test]
    fn confident_task_with_unscored_category_falls_back() {
        // Empty profiles: even a confident diagnosis has no backend to route to. The router
        // then runs its remaining heuristics and lands on the default.
        let report = build_report(
            "refactor the auth module to flatten the nesting",
            &auto_route_config(BackendId::Codex),
            &all_seeded_available(),
            &HashSet::new(),
            ProfileSet::default(),
        );

        assert_eq!(report.diagnosis.primary, TaskCategory::Refactor);
        assert!(!report.routing.from_profile);
        assert_eq!(report.routing.backend, BackendId::Codex);
        assert_eq!(report.routing.reason, "default");
    }

    #[test]
    fn report_serializes_to_expected_json_shape() {
        let report = build_report(
            "implement a function to parse the duration feature",
            &auto_route_config(BackendId::Opencode),
            &all_seeded_available(),
            &HashSet::new(),
            seeded_profiles(),
        );
        let json = serde_json::to_string(&report).unwrap();

        assert!(json.contains("\"task\":"));
        assert!(json.contains("\"primary\":\"coding\""));
        assert!(json.contains("\"reason\":\"profile\""));
        assert!(json.contains("\"backend\":\"claude\""));
        assert!(json.contains("\"available\":"));
    }

    #[test]
    fn human_render_contains_chain() {
        let report = build_report(
            "implement a function to parse the duration feature",
            &auto_route_config(BackendId::Opencode),
            &all_seeded_available(),
            &HashSet::new(),
            seeded_profiles(),
        );
        let out = render_human(&report);

        assert!(out.contains("features:"));
        assert!(out.contains("category:"));
        assert!(out.contains("coding"));
        assert!(out.contains("routing:"));
        assert!(out.contains("profile: coding score 88"));
    }
}
