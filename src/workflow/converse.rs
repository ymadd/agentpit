//! Conversation layer M1 — the ④ refute mechanism and ①③ note kinds (design §4.5).
//!
//! The inter-swarm conversation layer is deliberately **stateless and additive**: workers stay
//! one-shot exec legs, and the only durable substrate is the `events.jsonl` transcript ([`Note`]).
//! This module owns the two pieces shared by the CLI (`agentpit refute` / `agentpit note`) and the
//! MCP tools (`refute` / `post_note`) so neither channel re-implements the orchestration:
//!
//! - **④ refute** — a depth-guarded, ≥3-leg `critique → defense → adjudication` exchange. The
//!   surviving correction from the design debate (§4.4-c): a one-shot adversarial critique relayed
//!   *unrebutted* worsens a stuck swarm, so the critique is always followed by a defense leg before
//!   the manager adjudicates. Legs run **sequentially**, each a plain [`dispatch`] that inherits the
//!   workflow depth env (`guard.rs`) and the per-dispatch timeout. There is **no new state**: the
//!   defender reads the prior turn from the bundle this function returns, exactly as a stateless
//!   one-shot would read a transcript. Adjudication is the manager's own next turn, not a fourth
//!   dispatch.
//! - **①③ note kinds** — the two free-form `kind` tags a [`Note`](crate::events::Event::Note)
//!   carries: a 1→1 [`KIND_HANDOFF`] context pass, or a shared-board [`KIND_BOARD`] entry.
//!
//! [`Note`]: crate::events::Event::Note

use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::dispatch::{Registries, dispatch};
use crate::events::{LegStatus, RunLogger};
use crate::types::BackendId;

/// `kind` tag for a 1→1 handoff note: one leg passing context to the next (design ①).
pub const KIND_HANDOFF: &str = "handoff";
/// `kind` tag for a shared-board note: a scratch entry many legs may read (design ③).
pub const KIND_BOARD: &str = "board";

/// Normalize a free-form note `kind` to a known tag, defaulting to [`KIND_HANDOFF`]. The two
/// first-class tags are canonicalized case-insensitively; any other value passes through so a
/// manager can coin board sub-kinds. Shared by the CLI `agentpit note` and the MCP `post_note`.
pub fn normalize_kind(kind: Option<&str>) -> String {
    match kind.map(str::trim) {
        None | Some("") => KIND_HANDOFF.to_string(),
        Some(k) if k.eq_ignore_ascii_case(KIND_HANDOFF) => KIND_HANDOFF.to_string(),
        Some(k) if k.eq_ignore_ascii_case(KIND_BOARD) => KIND_BOARD.to_string(),
        Some(k) => k.to_string(),
    }
}

/// Resolve the critic/defender pair for a refutation. An explicitly named backend that is not
/// available is a hard error (the caller asked for something impossible); an omitted one is filled
/// from `preferred` (the adversarial-review members) and then any available backend, kept distinct
/// from the other leg when possible. With a single backend available the defender falls back to the
/// critic — a weaker self-refutation, but still one pass. Shared by the CLI and MCP `refute`.
pub fn resolve_pair(
    critic: Option<BackendId>,
    defender: Option<BackendId>,
    available: &HashSet<BackendId>,
    preferred: &[BackendId],
) -> Result<(BackendId, BackendId)> {
    let critic = match critic {
        Some(b) if available.contains(&b) => b,
        Some(b) => anyhow::bail!("critic backend {b} is not available"),
        None => pick(available, preferred, None)
            .ok_or_else(|| anyhow::anyhow!("no backend available to run the critique leg"))?,
    };
    let defender = match defender {
        Some(b) if available.contains(&b) => b,
        Some(b) => anyhow::bail!("defender backend {b} is not available"),
        None => pick(available, preferred, Some(critic)).unwrap_or(critic),
    };
    Ok((critic, defender))
}

/// Pick an available backend, preferring the `preferred` order and skipping `skip` when asked.
/// The fallback over the available set is sorted so the choice is deterministic even when the
/// preferred list does not cover the available backends.
fn pick(
    available: &HashSet<BackendId>,
    preferred: &[BackendId],
    skip: Option<BackendId>,
) -> Option<BackendId> {
    if let Some(b) = preferred
        .iter()
        .copied()
        .find(|b| available.contains(b) && Some(*b) != skip)
    {
        return Some(b);
    }
    let mut rest: Vec<BackendId> = available
        .iter()
        .copied()
        .filter(|b| Some(*b) != skip)
        .collect();
    rest.sort();
    rest.into_iter().next()
}

/// Byte cap applied to every candidate/critique embedded in a prompt and to every leg's output in
/// the rendered bundle. A stuck worker's candidate (or a verbose critic) could otherwise blow the
/// next leg's — or the manager's — context window. Matches the ensemble member cap.
const MAX_LEG_BYTES: usize = 48 * 1024;

/// Truncate `s` to at most `MAX_LEG_BYTES` bytes on a char boundary, appending a marker when cut.
fn clamp(s: &str) -> String {
    if s.len() <= MAX_LEG_BYTES {
        return s.to_string();
    }
    let mut end = MAX_LEG_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}\n[truncated: exceeded {MAX_LEG_BYTES} bytes]", &s[..end])
}

/// The outcome of the two dispatched legs of a refutation, ready for the manager to adjudicate.
/// `defense` is `None` only when the critique leg itself failed — there is then nothing to defend
/// against, so the defender is not dispatched.
#[derive(Debug, Clone)]
pub struct RefuteBundle {
    pub critic: BackendId,
    pub defender: BackendId,
    /// `Ok(critique)` or `Err(reason)` if the critic leg could not run.
    pub critique: Result<String, String>,
    /// `Some(Ok(defense))` / `Some(Err(reason))`, or `None` when the critique leg failed.
    pub defense: Option<Result<String, String>>,
}

/// Build the adversarial **critique** prompt for leg 1: find precisely why a stuck candidate is
/// wrong or incomplete. Pure and unit-tested.
pub fn critique_prompt(task: &str, candidate: &str) -> String {
    format!(
        "You are an ADVERSARIAL critic. A sub-task inside a larger workflow is STUCK: a worker \
         produced the candidate below, but the workflow cannot move forward on it. Find exactly \
         WHY — what is wrong, missing, or unproven — not to be balanced or reassuring.\n\
         \n\
         SUB-TASK (what the candidate was meant to achieve):\n\
         {task}\n\
         \n\
         CANDIDATE (the stuck worker's current output):\n\
         {candidate}\n\
         \n\
         Assume the candidate is broken until proven otherwise; default to skepticism, not charity. \
         Produce a SHORT, CONCRETE critique:\n\
         1. The single most likely reason it is stuck or wrong (one sentence).\n\
         2. Specific defects — incorrect claims, unhandled cases, missing steps, false assumptions — \
         each with a concrete scenario or counter-example, never \"could potentially\".\n\
         3. What the candidate would need to PROVE or ADD to become acceptable.\n\
         Do NOT rewrite the solution. Do NOT soften. If after honest scrutiny the candidate is \
         actually sound, say so explicitly and name the evidence that convinced you.",
        task = clamp(task),
        candidate = clamp(candidate),
    )
}

/// Build the **defense** prompt for leg 2: rebut the critique where it is wrong and fix the
/// candidate where it is right. This is the rebuttal turn the design (§4.4-c) makes mandatory so a
/// one-shot skeptic is never relayed unrebutted. Pure and unit-tested.
pub fn defense_prompt(task: &str, candidate: &str, critique: &str) -> String {
    format!(
        "You are DEFENDING a candidate solution against an adversarial critique. A sub-task is \
         stuck; a critic argued the candidate below is flawed. Respond HONESTLY: rebut the critique \
         where it is wrong, and FIX the candidate where the critique is right.\n\
         \n\
         SUB-TASK (what the candidate must achieve):\n\
         {task}\n\
         \n\
         CANDIDATE (the current output under dispute):\n\
         {candidate}\n\
         \n\
         CRITIQUE (the adversarial objections to answer):\n\
         {critique}\n\
         \n\
         Produce:\n\
         1. A point-by-point response: for each objection, either REBUT it (with concrete evidence \
         the critic was wrong) or CONCEDE it.\n\
         2. A REVISED candidate that survives the conceded objections — the smallest change that \
         makes it correct, not a rewrite for its own sake. If no change is warranted, restate the \
         candidate and explain why the critique does not land. Present the REVISED candidate in \
         the SAME FORMAT as the original candidate (e.g. a single fenced code block of the same \
         language, if the original was one) as the LAST thing in your response, so it is the one \
         block a reader — or an automated extractor — should treat as final.\n\
         Be concrete and specific. Do NOT invent agreement or disagreement to seem balanced.",
        task = clamp(task),
        candidate = clamp(candidate),
        critique = clamp(critique),
    )
}

/// Map a single dispatch into `Ok(output)` / `Err(reason)`, treating an auth failure as a leg
/// failure (not a panic). Each leg runs under `cancel` and inherits the per-dispatch timeout. When
/// a `logger` is supplied the leg brackets the dispatch with `member_started`/`member_finished`, so
/// the refutation shows in the dashboard swarm as a live two-member run; without one it is silent
/// (the pure path the unit tests and any non-observed caller take).
async fn run_leg(
    backend: BackendId,
    prompt: &str,
    cwd: &Path,
    cancel: CancellationToken,
    regs: &Registries,
    logger: Option<&RunLogger>,
) -> Result<String, String> {
    if let Some(l) = logger {
        l.member_started(backend, false);
    }
    let started = Instant::now();
    let sink: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(|_chunk: &str| {});
    let outcome = match dispatch(backend, prompt, cwd, cancel, sink, regs).await {
        Ok(res) if res.auth_failed => Err(format!("{backend}: auth failure during execution")),
        Ok(res) => Ok(res.output.trim().to_string()),
        Err(e) => Err(format!("{backend}: {e:#}")),
    };
    if let Some(l) = logger {
        let elapsed = started.elapsed().as_millis() as u64;
        match &outcome {
            Ok(text) => l.member_finished(
                backend,
                false,
                LegStatus::Ok,
                elapsed,
                Some(text.len()),
                None,
            ),
            Err(reason) => l.member_finished(
                backend,
                false,
                LegStatus::Error,
                elapsed,
                None,
                Some(reason.clone()),
            ),
        }
    }
    outcome
}

/// Run the ④ refutation: dispatch the critic (leg 1), then — only if the critique succeeded —
/// dispatch the defender carrying that critique (leg 2). Returns both for the manager to
/// adjudicate (leg 3). Sequential by design: the defender must read the critique. Never panics and
/// never aborts the caller's run — a failed leg is reported in the bundle, mirroring the
/// member-failure tolerance of ensembles, because refutation is advisory.
#[allow(clippy::too_many_arguments)]
pub async fn run_refute(
    task: &str,
    candidate: &str,
    critic: BackendId,
    defender: BackendId,
    cwd: &Path,
    regs: &Registries,
    cancel: CancellationToken,
    logger: Option<&RunLogger>,
) -> RefuteBundle {
    let critique = run_leg(
        critic,
        &critique_prompt(task, candidate),
        cwd,
        cancel.clone(),
        regs,
        logger,
    )
    .await;

    let defense = match &critique {
        Ok(text) => Some(
            run_leg(
                defender,
                &defense_prompt(task, candidate, text),
                cwd,
                cancel,
                regs,
                logger,
            )
            .await,
        ),
        // No critique ⇒ nothing to defend against; do not spend the defender leg. Mark the
        // defender skipped so a finished run shows it as skipped rather than perpetually "pending"
        // — but only when it is a distinct backend, since a self-refute shares one member row whose
        // critique-failure status must not be overwritten.
        Err(_) => {
            if let Some(l) = logger
                && defender != critic
            {
                l.member_finished(defender, false, LegStatus::Skipped, 0, None, None);
            }
            None
        }
    };

    RefuteBundle {
        critic,
        defender,
        critique,
        defense,
    }
}

/// Render a [`RefuteBundle`] into the manager-facing transcript the adjudicator (leg 3) reads:
/// the critique, the defense, and an explicit adjudication instruction. Each leg's text is clamped.
pub fn render_refute(bundle: &RefuteBundle) -> String {
    let critique = match &bundle.critique {
        Ok(text) => clamp(text),
        Err(reason) => format!("(critique leg failed: {reason})"),
    };
    let defense = match &bundle.defense {
        Some(Ok(text)) => clamp(text),
        Some(Err(reason)) => format!("(defense leg failed: {reason})"),
        None => {
            "(defense skipped: the critique leg failed, so there was nothing to defend)".to_string()
        }
    };
    format!(
        "=== REFUTE: critique → defense (adjudicate below) ===\n\
         critic={critic}  defender={defender}\n\
         \n\
         --- CRITIQUE [{critic}] ---\n\
         {critique}\n\
         \n\
         --- DEFENSE [{defender}] ---\n\
         {defense}\n\
         \n\
         --- ADJUDICATION (you, the manager) ---\n\
         Weigh the critique against the defense and decide: ADOPT the revised candidate, KEEP the \
         original, or DISCARD and re-plan. State your verdict and a one-line reason, then continue \
         the workflow. Do this ONCE before discarding a stuck sub-task — do not loop.",
        critic = bundle.critic,
        defender = bundle.defender,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn critique_prompt_embeds_task_and_candidate_and_demands_concreteness() {
        let p = critique_prompt("make auth idempotent", "retry without a guard");
        assert!(p.contains("make auth idempotent"));
        assert!(p.contains("retry without a guard"));
        assert!(p.contains("ADVERSARIAL"));
        assert!(p.contains("never \"could potentially\""));
        // It must NOT ask the critic to rewrite the solution.
        assert!(p.contains("Do NOT rewrite"));
    }

    #[test]
    fn defense_prompt_carries_the_critique_for_a_real_rebuttal() {
        let p = defense_prompt("goal", "candidate text", "the critique points");
        assert!(p.contains("goal"));
        assert!(p.contains("candidate text"));
        assert!(p.contains("the critique points"));
        assert!(p.contains("REBUT"));
        assert!(p.contains("CONCEDE"));
        assert!(p.contains("REVISED candidate"));
    }

    #[test]
    fn defense_prompt_demands_the_revision_in_the_original_format_as_the_final_block() {
        // So a bench/manager extractor (e.g. `extract_last_fence`) can reliably pull the revised
        // candidate out of free-form defense prose instead of guessing which block is final.
        let p = defense_prompt("goal", "candidate", "critique");
        assert!(p.contains("SAME FORMAT as the original candidate"));
        assert!(p.contains("LAST thing in your response"));
    }

    #[test]
    fn clamp_truncates_oversized_input_on_a_boundary() {
        let big = "x".repeat(MAX_LEG_BYTES * 2);
        let out = clamp(&big);
        assert!(out.len() < big.len());
        assert!(out.contains("[truncated"));
        // A short string is returned untouched.
        assert_eq!(clamp("short"), "short");
    }

    #[test]
    fn render_includes_both_legs_and_the_adjudication_instruction() {
        let bundle = RefuteBundle {
            critic: BackendId::Codex,
            defender: BackendId::Gemini,
            critique: Ok("the flaw is X".into()),
            defense: Some(Ok("X is wrong because Y; revised: Z".into())),
        };
        let text = render_refute(&bundle);
        assert!(text.contains("CRITIQUE [codex]"));
        assert!(text.contains("the flaw is X"));
        assert!(text.contains("DEFENSE [gemini]"));
        assert!(text.contains("revised: Z"));
        assert!(text.contains("ADJUDICATION"));
        assert!(text.contains("ADOPT"));
        assert!(text.contains("DISCARD"));
    }

    #[test]
    fn render_explains_a_skipped_defense_when_the_critique_failed() {
        let bundle = RefuteBundle {
            critic: BackendId::Codex,
            defender: BackendId::Gemini,
            critique: Err("codex: not authenticated".into()),
            defense: None,
        };
        let text = render_refute(&bundle);
        assert!(text.contains("critique leg failed: codex: not authenticated"));
        assert!(text.contains("defense skipped"));
    }

    #[test]
    fn note_kind_constants_are_the_expected_tags() {
        assert_eq!(KIND_HANDOFF, "handoff");
        assert_eq!(KIND_BOARD, "board");
    }

    #[test]
    fn normalize_kind_defaults_to_handoff_and_canonicalizes_known_tags() {
        assert_eq!(normalize_kind(None), KIND_HANDOFF);
        assert_eq!(normalize_kind(Some("")), KIND_HANDOFF);
        assert_eq!(normalize_kind(Some("  ")), KIND_HANDOFF);
        assert_eq!(normalize_kind(Some("HANDOFF")), KIND_HANDOFF);
        assert_eq!(normalize_kind(Some("Board")), KIND_BOARD);
        // An unrecognized kind passes through verbatim.
        assert_eq!(normalize_kind(Some("decision")), "decision");
    }

    fn set(items: &[BackendId]) -> HashSet<BackendId> {
        items.iter().copied().collect()
    }

    #[test]
    fn pick_prefers_the_preferred_order_and_respects_skip() {
        let available = set(&[BackendId::Codex, BackendId::Antigravity, BackendId::Gemini]);
        let preferred = [BackendId::Codex, BackendId::Antigravity];
        assert_eq!(pick(&available, &preferred, None), Some(BackendId::Codex));
        assert_eq!(
            pick(&available, &preferred, Some(BackendId::Codex)),
            Some(BackendId::Antigravity)
        );
    }

    #[test]
    fn pick_falls_back_deterministically_when_preferred_is_exhausted() {
        let available = set(&[BackendId::Gemini, BackendId::Opencode]);
        let preferred = [BackendId::Codex, BackendId::Antigravity];
        // Neither preferred is available → the sorted-available fallback is deterministic.
        let first = pick(&available, &preferred, None);
        assert_eq!(first, pick(&available, &preferred, None));
        assert!(first.is_some());
        assert_eq!(pick(&HashSet::new(), &preferred, None), None);
    }

    #[test]
    fn resolve_pair_defaults_to_distinct_preferred_backends() {
        let available = set(&[BackendId::Codex, BackendId::Antigravity, BackendId::Gemini]);
        let preferred = [BackendId::Codex, BackendId::Antigravity];
        let (critic, defender) = resolve_pair(None, None, &available, &preferred).unwrap();
        assert_eq!(critic, BackendId::Codex);
        assert_eq!(defender, BackendId::Antigravity);
        assert_ne!(critic, defender);
    }

    #[test]
    fn resolve_pair_honors_explicit_choices() {
        let available = set(&[BackendId::Codex, BackendId::Gemini]);
        let preferred = [BackendId::Codex];
        let (critic, defender) = resolve_pair(
            Some(BackendId::Gemini),
            Some(BackendId::Codex),
            &available,
            &preferred,
        )
        .unwrap();
        assert_eq!(critic, BackendId::Gemini);
        assert_eq!(defender, BackendId::Codex);
    }

    #[test]
    fn resolve_pair_rejects_an_unavailable_explicit_backend() {
        let available = set(&[BackendId::Codex]);
        let preferred = [BackendId::Codex];
        assert!(resolve_pair(Some(BackendId::Gemini), None, &available, &preferred).is_err());
        assert!(resolve_pair(None, Some(BackendId::Gemini), &available, &preferred).is_err());
    }

    #[test]
    fn resolve_pair_falls_back_to_self_with_a_single_backend() {
        let available = set(&[BackendId::Codex]);
        let preferred = [BackendId::Codex, BackendId::Antigravity];
        let (critic, defender) = resolve_pair(None, None, &available, &preferred).unwrap();
        assert_eq!(critic, BackendId::Codex);
        assert_eq!(defender, BackendId::Codex);
    }

    #[test]
    fn resolve_pair_errors_when_nothing_is_available() {
        assert!(resolve_pair(None, None, &HashSet::new(), &[BackendId::Codex]).is_err());
    }
}
