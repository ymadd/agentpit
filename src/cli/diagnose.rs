//! `agentpit diagnose "<task>" [--json]` — a dry-run observation point (design §1.5).
//!
//! Shows the full diagnosis chain: extracted features → diagnosed `(category, confidence)`
//! → the backend the capability profiles would route the task to, and why. `--json` emits a
//! machine-readable verdict for downstream automation (the Phase B issue→routing GitHub
//! Action).
//!
//! The backend selection mirrors the profile stage of `router::Router::resolve`: a confident
//! diagnosis routes to the highest-scoring available backend for the category; a shaky
//! verdict (confidence below the LLM-assist threshold) or a category no available backend has
//! scored falls back to the configured default — never steering work to an odd backend on a
//! weak guess.

use std::collections::HashSet;

use anyhow::Result;
use console::style;
use serde::Serialize;

use crate::diagnose::{self, DiagnoseMethod, Diagnosis, LLM_ASSIST_CONFIDENCE_THRESHOLD};
use crate::profile::{ProfileSet, TaskCategory, load_profiles};
use crate::types::BackendId;

use super::load_context;

/// Why a task landed on its backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum RoutingReason {
    /// Confident diagnosis matched the highest-scoring available backend for the category.
    Profile,
    /// Diagnosis confidence is below the LLM-assist threshold; profile routing is skipped and
    /// the task falls back to the default backend.
    LowConfidence,
    /// Confident diagnosis, but no available backend has scored the category; falls back.
    NoProfileMatch,
}

/// The routing verdict for a diagnosed task.
#[derive(Debug, Clone, Serialize)]
struct Routing {
    /// The backend the task would be sent to (the profile pick, or the default fallback).
    backend: BackendId,
    reason: RoutingReason,
    /// True when `backend` is the profile argmax; false when it is the default fallback.
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
    threshold: f32,
}

pub async fn run(task: String, json: bool) -> Result<()> {
    let ctx = load_context()?;
    let available = ctx.regs.available();
    let profiles = load_profiles(None)?;
    let default_backend = ctx.loaded.config.default.backend;

    let report = build_report(&task, &profiles, &available, default_backend);

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", render_human(&report));
    }
    Ok(())
}

/// Build the report. Pure: diagnoses the task and resolves the profile routing without any
/// I/O, returning a fresh `DiagnoseReport`.
fn build_report(
    task: &str,
    profiles: &ProfileSet,
    available: &HashSet<BackendId>,
    default_backend: BackendId,
) -> DiagnoseReport {
    let diagnosis = diagnose::diagnose(task);
    let routing = route(&diagnosis, profiles, available, default_backend);

    let mut available_sorted: Vec<BackendId> = available.iter().copied().collect();
    available_sorted.sort();

    DiagnoseReport {
        task: task.to_string(),
        diagnosis,
        routing,
        available: available_sorted,
        threshold: LLM_ASSIST_CONFIDENCE_THRESHOLD,
    }
}

/// Resolve the routing verdict, mirroring the profile stage of `router::Router::resolve`.
fn route(
    diagnosis: &Diagnosis,
    profiles: &ProfileSet,
    available: &HashSet<BackendId>,
    default_backend: BackendId,
) -> Routing {
    if diagnosis.confidence < LLM_ASSIST_CONFIDENCE_THRESHOLD {
        return Routing {
            backend: default_backend,
            reason: RoutingReason::LowConfidence,
            from_profile: false,
            category: None,
            score: None,
        };
    }

    match profiles.best_for(diagnosis.primary, available) {
        Some((backend, score)) => Routing {
            backend,
            reason: RoutingReason::Profile,
            from_profile: true,
            category: Some(diagnosis.primary),
            score: Some(score.value),
        },
        None => Routing {
            backend: default_backend,
            reason: RoutingReason::NoProfileMatch,
            from_profile: false,
            category: Some(diagnosis.primary),
            score: None,
        },
    }
}

fn method_str(method: DiagnoseMethod) -> &'static str {
    match method {
        DiagnoseMethod::Heuristic => "heuristic",
        DiagnoseMethod::LlmAssisted => "llm_assisted",
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
    let detail = match r.reason {
        RoutingReason::Profile => format!(
            "profile: {} score {}",
            r.category.map(|c| c.as_str()).unwrap_or("?"),
            r.score.unwrap_or(0)
        ),
        RoutingReason::LowConfidence => format!(
            "default — confidence {:.2} < {:.2}, diagnosis too weak for profile routing",
            d.confidence, report.threshold
        ),
        RoutingReason::NoProfileMatch => format!(
            "default — no available backend has scored {}",
            r.category.map(|c| c.as_str()).unwrap_or("?")
        ),
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
            BackendId::Gemini,
            BackendId::Opencode,
        ]
        .into_iter()
        .collect()
    }

    #[test]
    fn confident_coding_task_routes_to_profile_argmax() {
        let profiles = seeded_profiles();
        let available = all_seeded_available();
        let report = build_report(
            "implement a function to parse the duration feature",
            &profiles,
            &available,
            BackendId::Opencode,
        );

        assert_eq!(report.diagnosis.primary, TaskCategory::Coding);
        assert!(report.diagnosis.confidence >= LLM_ASSIST_CONFIDENCE_THRESHOLD);
        assert_eq!(report.routing.reason, RoutingReason::Profile);
        assert!(report.routing.from_profile);
        // Claude is the seeded coding argmax.
        assert_eq!(report.routing.backend, BackendId::Claude);
        assert_eq!(report.routing.score, Some(88));
    }

    #[test]
    fn low_confidence_task_falls_back_to_default() {
        let profiles = seeded_profiles();
        let available = all_seeded_available();
        let report = build_report("alpha beta gamma", &profiles, &available, BackendId::Gemini);

        assert!(report.diagnosis.confidence < LLM_ASSIST_CONFIDENCE_THRESHOLD);
        assert_eq!(report.routing.reason, RoutingReason::LowConfidence);
        assert!(!report.routing.from_profile);
        assert_eq!(report.routing.backend, BackendId::Gemini);
        assert_eq!(report.routing.score, None);
    }

    #[test]
    fn confident_task_with_unscored_category_falls_back() {
        // Empty profiles: even a confident diagnosis has no backend to route to.
        let profiles = ProfileSet::default();
        let available = all_seeded_available();
        let report = build_report(
            "refactor the auth module to flatten the nesting",
            &profiles,
            &available,
            BackendId::Codex,
        );

        assert_eq!(report.diagnosis.primary, TaskCategory::Refactor);
        assert_eq!(report.routing.reason, RoutingReason::NoProfileMatch);
        assert!(!report.routing.from_profile);
        assert_eq!(report.routing.backend, BackendId::Codex);
        assert_eq!(report.routing.category, Some(TaskCategory::Refactor));
    }

    #[test]
    fn report_serializes_to_expected_json_shape() {
        let profiles = seeded_profiles();
        let available = all_seeded_available();
        let report = build_report(
            "implement a function to parse the duration feature",
            &profiles,
            &available,
            BackendId::Opencode,
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
        let profiles = seeded_profiles();
        let available = all_seeded_available();
        let report = build_report(
            "implement a function to parse the duration feature",
            &profiles,
            &available,
            BackendId::Opencode,
        );
        let out = render_human(&report);

        assert!(out.contains("features:"));
        assert!(out.contains("category:"));
        assert!(out.contains("coding"));
        assert!(out.contains("routing:"));
        assert!(out.contains("profile: coding score 88"));
    }
}
