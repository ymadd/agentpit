//! Role resolution — the layer between configured `[workflow.roles.<name>]` personas and the
//! backends that play them.
//!
//! Roles fix the CAST, never the SCRIPT: the manager still improvises the decomposition and
//! ordering, but WHO runs a sub-task moves from LLM whim into config. Resolution follows the
//! same "preference order → deterministic availability fallback" shape as
//! [`converse::pick`](super::converse) so backend choice stays reproducible.
//!
//! The reserved role name [`MANAGER_ROLE`] configures the orchestrator itself; every other role
//! is a worker the manager may dispatch to (`rescue --role <name>` / `dispatch_task {role}`).
//! With no roles configured the workflow keeps the legacy flat-backend roster — this module is
//! only consulted when the user opted into roles.

use std::collections::BTreeMap;

use anyhow::Result;

use crate::config::RoleConfig;
use crate::types::BackendId;

/// The reserved role name that configures the workflow orchestrator itself.
pub const MANAGER_ROLE: &str = "manager";

/// A worker role resolved against the currently available backends: the persona plus the one
/// backend that will play it for this dispatch.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedRole {
    pub name: String,
    pub backend: BackendId,
    pub prompt: Option<String>,
    /// The role's configured model, if any (`[workflow.roles.<name>].model`). Threaded to the
    /// backend CLI's model flag at dispatch, unless an explicit `--model` overrides it.
    pub model: Option<String>,
}

/// The manager role's contribution to a workflow run. `backend` is `None` when the role only
/// carries a persona (empty `backends`), in which case the legacy manager resolution
/// (`--manager` → `[workflow].manager_backend` → default) still picks the backend.
#[derive(Debug, Clone, PartialEq)]
pub struct ManagerRole {
    pub backend: Option<BackendId>,
    pub prompt: Option<String>,
    /// The manager role's configured model (`[workflow.roles.manager].model`), if any.
    pub model: Option<String>,
}

/// Resolve a WORKER role by name against the available backends: the first entry of the role's
/// preference list that is available wins; an empty list falls back to the sorted available set
/// (deterministic, mirroring `converse::pick`). Unknown names, the reserved manager name, and a
/// preference list with no available member are hard errors — the caller asked for something
/// impossible and silently substituting a backend would defeat the point of configured casting.
pub fn resolve_role(
    name: &str,
    roles: &BTreeMap<String, RoleConfig>,
    available: &[BackendId],
) -> Result<ResolvedRole> {
    if name == MANAGER_ROLE {
        anyhow::bail!(
            "the reserved role '{MANAGER_ROLE}' configures the orchestrator; \
             dispatch to a worker role instead"
        );
    }
    let Some(role) = roles.get(name) else {
        let known = worker_roles(roles)
            .map(|(n, _)| n.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        if known.is_empty() {
            anyhow::bail!("unknown role '{name}': no worker roles are configured");
        }
        anyhow::bail!("unknown role '{name}'. Configured worker roles: {known}");
    };
    let backend = if role.backends.is_empty() {
        let mut sorted = available.to_vec();
        sorted.sort();
        sorted
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("role '{name}': no backend is available to play it"))?
    } else {
        role.backends
            .iter()
            .copied()
            .find(|b| available.contains(b))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "role '{name}': none of its backends ({}) are available",
                    csv(&role.backends)
                )
            })?
    };
    Ok(ResolvedRole {
        name: name.to_string(),
        backend,
        prompt: role.prompt.clone(),
        model: role.model.clone(),
    })
}

/// Resolve the reserved `manager` role, if configured. `supported` is the manager-capability
/// predicate (today `exec::is_supported_manager`: claude|codex). Returns:
/// - `Ok(None)` — no manager role configured; legacy resolution applies untouched.
/// - `Ok(Some(_))` — the role exists; `backend` is the first SUPPORTED entry of its preference
///   list (or `None` when the list is empty and only the persona applies).
/// - `Err` — the user explicitly listed manager backends but none can act as a manager.
pub fn resolve_manager(
    roles: &BTreeMap<String, RoleConfig>,
    supported: impl Fn(BackendId) -> bool,
) -> Result<Option<ManagerRole>> {
    let Some(role) = roles.get(MANAGER_ROLE) else {
        return Ok(None);
    };
    let backend = if role.backends.is_empty() {
        None
    } else {
        Some(
            role.backends
                .iter()
                .copied()
                .find(|b| supported(*b))
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "[workflow.roles.manager]: none of its backends ({}) can act as a \
                         manager (supported: claude, codex)",
                        csv(&role.backends)
                    )
                })?,
        )
    };
    Ok(Some(ManagerRole {
        backend,
        prompt: role.prompt.clone(),
        model: role.model.clone(),
    }))
}

/// Resolve the effective model for a dispatch by precedence: an explicit `--model` wins, then the
/// role's `model`, then the backend's `[backends.<id>].model` default; `None` = the CLI's own
/// default (no `--model` flag emitted). Pure so the CLI and MCP layers share one rule.
pub fn resolve_model(
    explicit: Option<&str>,
    role_model: Option<&str>,
    backend_default: Option<&str>,
) -> Option<String> {
    explicit
        .or(role_model)
        .or(backend_default)
        .map(str::to_string)
}

/// Iterate the WORKER roles (every configured role except the reserved manager), in the map's
/// deterministic name order. The manager prompt's roster and role validation both build on this.
pub fn worker_roles(
    roles: &BTreeMap<String, RoleConfig>,
) -> impl Iterator<Item = (&String, &RoleConfig)> {
    roles
        .iter()
        .filter(|(name, _)| name.as_str() != MANAGER_ROLE)
}

/// Wrap a dispatched sub-task in the role's persona preamble. Without a persona — `None` OR a
/// blank/whitespace-only prompt — the task passes through untouched, so a prompt-less role costs
/// nothing. Treating blank as absent keeps this consistent with [`summary_line`]'s "(no persona)"
/// and defends against a stray `prompt = ""` (e.g. one an older dashboard build may have written)
/// producing an empty `=== ROLE: … ===` header around the task.
pub fn persona_task(role_name: &str, prompt: Option<&str>, task: &str) -> String {
    match prompt.map(str::trim).filter(|p| !p.is_empty()) {
        None => task.to_string(),
        Some(p) => format!("=== ROLE: {role_name} ===\n{p}\n\n=== TASK ===\n{task}"),
    }
}

/// One-line persona summary for the manager-facing roster: the first non-empty prompt line,
/// truncated to at most `MAX_SUMMARY_CHARS` characters on a char boundary.
pub fn summary_line(role: &RoleConfig) -> String {
    const MAX_SUMMARY_CHARS: usize = 80;
    let Some(prompt) = role.prompt.as_deref() else {
        return "(no persona)".to_string();
    };
    let first = prompt
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        // A blank/whitespace-only prompt has no non-empty line — report it as no persona, matching
        // persona_task's blank-is-passthrough treatment so the roster and the dispatch agree.
        .unwrap_or("(no persona)");
    if first.chars().count() <= MAX_SUMMARY_CHARS {
        return first.to_string();
    }
    let truncated: String = first.chars().take(MAX_SUMMARY_CHARS).collect();
    format!("{truncated}…")
}

fn csv(backends: &[BackendId]) -> String {
    backends
        .iter()
        .map(BackendId::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles(entries: &[(&str, &[BackendId], Option<&str>)]) -> BTreeMap<String, RoleConfig> {
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
    fn resolves_first_available_backend_in_preference_order() {
        let r = roles(&[(
            "reviewer",
            &[BackendId::Codex, BackendId::Antigravity],
            Some("strict"),
        )]);
        // Codex unavailable → antigravity wins.
        let resolved =
            resolve_role("reviewer", &r, &[BackendId::Antigravity, BackendId::Gemini]).unwrap();
        assert_eq!(resolved.backend, BackendId::Antigravity);
        assert_eq!(resolved.prompt.as_deref(), Some("strict"));
        // Both available → preference order wins over availability order.
        let resolved =
            resolve_role("reviewer", &r, &[BackendId::Antigravity, BackendId::Codex]).unwrap();
        assert_eq!(resolved.backend, BackendId::Codex);
    }

    #[test]
    fn empty_backend_list_falls_back_to_sorted_available() {
        let r = roles(&[("researcher", &[], None)]);
        let a = resolve_role("researcher", &r, &[BackendId::Opencode, BackendId::Gemini]).unwrap();
        let b = resolve_role("researcher", &r, &[BackendId::Gemini, BackendId::Opencode]).unwrap();
        // Deterministic regardless of the available set's order.
        assert_eq!(a.backend, b.backend);
    }

    #[test]
    fn unknown_role_lists_configured_workers() {
        let r = roles(&[
            ("reviewer", &[BackendId::Codex], None),
            (MANAGER_ROLE, &[BackendId::Claude], None),
        ]);
        let err = resolve_role("ghost", &r, &[BackendId::Codex])
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown role 'ghost'"));
        assert!(err.contains("reviewer"));
        // The manager is not offered as a dispatch target.
        assert!(!err.contains("manager"));
    }

    #[test]
    fn unknown_role_with_no_worker_roles_configured_says_so() {
        // Covers the `known.is_empty()` branch: a first-touch user running `rescue --role x` with
        // no `[workflow.roles]` at all (or only a manager role) gets the "no worker roles are
        // configured" wording, not an empty "Configured worker roles: " list.
        let empty = BTreeMap::new();
        let err = resolve_role("x", &empty, &[BackendId::Codex])
            .unwrap_err()
            .to_string();
        assert!(err.contains("no worker roles are configured"), "got: {err}");
        // A manager-only config still has zero WORKER roles → same message.
        let manager_only = roles(&[(MANAGER_ROLE, &[BackendId::Claude], None)]);
        let err = resolve_role("x", &manager_only, &[BackendId::Claude])
            .unwrap_err()
            .to_string();
        assert!(err.contains("no worker roles are configured"), "got: {err}");
    }

    #[test]
    fn dispatching_to_the_manager_role_is_rejected() {
        let r = roles(&[(MANAGER_ROLE, &[BackendId::Claude], None)]);
        let err = resolve_role(MANAGER_ROLE, &r, &[BackendId::Claude])
            .unwrap_err()
            .to_string();
        assert!(err.contains("reserved role"));
    }

    #[test]
    fn no_available_backend_for_a_role_is_a_hard_error() {
        let r = roles(&[("reviewer", &[BackendId::Codex], None)]);
        let err = resolve_role("reviewer", &r, &[BackendId::Gemini])
            .unwrap_err()
            .to_string();
        assert!(err.contains("codex"));
        // Empty preference list + nothing available at all.
        let r = roles(&[("any", &[], None)]);
        assert!(resolve_role("any", &r, &[]).is_err());
    }

    #[test]
    fn manager_role_absent_yields_none() {
        let r = roles(&[("reviewer", &[BackendId::Codex], None)]);
        assert_eq!(resolve_manager(&r, |_| true).unwrap(), None);
    }

    #[test]
    fn manager_role_picks_first_supported_backend() {
        let r = roles(&[(
            MANAGER_ROLE,
            &[BackendId::Gemini, BackendId::Codex],
            Some("plan tightly"),
        )]);
        // Gemini cannot manage → codex wins.
        let m = resolve_manager(&r, |b| matches!(b, BackendId::Claude | BackendId::Codex))
            .unwrap()
            .unwrap();
        assert_eq!(m.backend, Some(BackendId::Codex));
        assert_eq!(m.prompt.as_deref(), Some("plan tightly"));
    }

    #[test]
    fn manager_role_with_no_supported_backend_errors() {
        let r = roles(&[(MANAGER_ROLE, &[BackendId::Gemini], None)]);
        let err = resolve_manager(&r, |_| false).unwrap_err().to_string();
        assert!(err.contains("gemini"));
        assert!(err.contains("supported: claude, codex"));
    }

    #[test]
    fn manager_role_persona_only_keeps_legacy_backend_resolution() {
        let r = roles(&[(MANAGER_ROLE, &[], Some("persona only"))]);
        let m = resolve_manager(&r, |_| true).unwrap().unwrap();
        assert_eq!(m.backend, None);
        assert_eq!(m.prompt.as_deref(), Some("persona only"));
    }

    #[test]
    fn worker_roles_excludes_the_manager() {
        let r = roles(&[
            (MANAGER_ROLE, &[BackendId::Claude], None),
            ("implementer", &[BackendId::Claude], None),
            ("reviewer", &[BackendId::Codex], None),
        ]);
        let names: Vec<&str> = worker_roles(&r).map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["implementer", "reviewer"]);
    }

    #[test]
    fn persona_task_wraps_only_when_a_prompt_exists() {
        assert_eq!(persona_task("reviewer", None, "check this"), "check this");
        let wrapped = persona_task("reviewer", Some("Be strict.\n"), "check this");
        assert!(wrapped.starts_with("=== ROLE: reviewer ===\nBe strict."));
        assert!(wrapped.ends_with("=== TASK ===\ncheck this"));
    }

    #[test]
    fn persona_task_treats_blank_prompt_as_passthrough() {
        // A stray `prompt = ""` (or whitespace-only) must not produce an empty ROLE header — it
        // is passthrough, consistent with summary_line reporting "(no persona)" for the same.
        assert_eq!(persona_task("reviewer", Some(""), "t"), "t");
        assert_eq!(persona_task("reviewer", Some("   \n\t"), "t"), "t");
        let blank = RoleConfig {
            backends: vec![],
            prompt: Some("   ".into()),
            model: None,
        };
        assert_eq!(summary_line(&blank), "(no persona)");
    }

    #[test]
    fn summary_line_takes_first_nonempty_line_and_truncates() {
        let role = RoleConfig {
            backends: vec![],
            prompt: Some("\n\n  You review code.  \nSecond line.".into()),
            model: None,
        };
        assert_eq!(summary_line(&role), "You review code.");
        let long = RoleConfig {
            backends: vec![],
            prompt: Some("あ".repeat(100)),
            model: None,
        };
        let s = summary_line(&long);
        assert!(s.ends_with('…'));
        assert_eq!(s.chars().count(), 81);
        let none = RoleConfig {
            backends: vec![],
            prompt: None,
            model: None,
        };
        assert_eq!(summary_line(&none), "(no persona)");
    }

    #[test]
    fn resolve_role_carries_model_and_resolve_model_follows_precedence() {
        let mut r = BTreeMap::new();
        r.insert(
            "reviewer".to_string(),
            RoleConfig {
                backends: vec![BackendId::Codex],
                prompt: Some("x".into()),
                model: Some("gpt-5-codex".into()),
            },
        );
        let resolved = resolve_role("reviewer", &r, &[BackendId::Codex]).unwrap();
        assert_eq!(resolved.model.as_deref(), Some("gpt-5-codex"));

        // Precedence: explicit --model > role.model > backend default > None.
        assert_eq!(
            resolve_model(Some("opus"), Some("rm"), Some("bm")).as_deref(),
            Some("opus")
        );
        assert_eq!(
            resolve_model(None, Some("rm"), Some("bm")).as_deref(),
            Some("rm")
        );
        assert_eq!(resolve_model(None, None, Some("bm")).as_deref(), Some("bm"));
        assert_eq!(resolve_model(None, None, None), None);
    }
}
