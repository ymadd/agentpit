//! Central declaration of how much autonomy each exec backend is granted.
//!
//! Every exec backend in agentpit runs **non-interactively** — there is no human at a TTY
//! to answer permission prompts — so each CLI is launched with the flag that lets it act
//! without one: `agy --dangerously-skip-permissions`, `gemini --yolo --skip-trust`,
//! `claude --permission-mode auto` (see [`claude_permission_mode`]), or, for `codex exec`,
//! the inherently non-interactive `exec` subcommand. That posture is a real security
//! decision — the backend may read, edit, and run tools in `cwd` without confirmation.
//!
//! Not every one of those flags *skips* the gate. Claude's `auto` routes each action past a
//! separate classifier model instead of past a human, so the gate still exists — it just
//! has no TTY in it. The distinction matters when reading [`AutonomyLevel::FullAutonomy`]
//! below: it describes what agentpit grants, not what the CLI does with the grant.
//!
//! Rather than leave that reasoning scattered across each adapter's magic flags, every
//! [`ExecAdapter`](super::ExecAdapter) declares its [`AutonomyLevel`] in one auditable
//! place, and each adapter's unit test asserts the dangerous flags only appear where
//! [`AutonomyLevel::FullAutonomy`] is declared. (ACP backends like opencode negotiate
//! permissions through the ACP handler instead; this enum covers exec backends only.)

/// The permission posture an exec backend is launched with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutonomyLevel {
    /// The backend may read, edit, and execute in `cwd` without per-action approval. All
    /// exec backends run this way today because they are spawned non-interactively.
    FullAutonomy,
    /// The backend prompts for permission before acting. No exec backend uses this today;
    /// it exists so a future "safe mode" can be expressed here rather than by
    /// re-scattering permission flags across adapters.
    Prompted,
}

impl AutonomyLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            AutonomyLevel::FullAutonomy => "full-autonomy",
            AutonomyLevel::Prompted => "prompted",
        }
    }
}

/// The `--permission-mode` a `claude` dispatch is launched with, given the model it is
/// pinned to (`None` = the CLI's own default).
///
/// `auto` is the mode to want here: a separate classifier model reviews every action before
/// it runs — blocking what escalates beyond the request, targets unrecognized
/// infrastructure, or looks driven by content the agent just read — and it needs no TTY, so
/// it fits a non-interactive dispatch exactly. It is strictly broader than `acceptEdits`,
/// which auto-approves file edits and a short list of filesystem commands and leaves every
/// other shell command and network request to be pre-approved by rule.
///
/// The classifier is not available on every model. Anthropic documents Opus 4.6+, Sonnet
/// 4.6+, and Fable 5 as supported, and names Sonnet 4.5, Opus 4.5, every Haiku, and the
/// claude-3 family as unsupported on every provider (permission-modes docs, read
/// 2026-08-15). A run pinned to one of those keeps the `acceptEdits` posture agentpit
/// shipped before rather than asking for a mode that account cannot have.
///
/// An organization can disable auto mode in managed settings. That machine-level policy
/// cannot be inferred from a model id, so deployments can set
/// `AGENTPIT_CLAUDE_PERMISSION_MODE=acceptEdits`. Only the two non-interactive, non-bypass
/// modes used here are accepted; an invalid value falls back to model detection.
pub fn claude_permission_mode(model: Option<&str>) -> &'static str {
    if let Ok(value) = std::env::var("AGENTPIT_CLAUDE_PERMISSION_MODE")
        && let Some(mode) = permission_mode_override(&value)
    {
        return mode;
    }
    match model {
        Some(m) if !classifier_supports(m) => "acceptEdits",
        // An alias (`opus`, `sonnet`) resolves to a current model, and no model at all means
        // the CLI's configured default — both land on the supported side.
        _ => "auto",
    }
}

fn permission_mode_override(value: &str) -> Option<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "auto" => Some("auto"),
        "acceptedits" | "accept_edits" | "accept-edits" => Some("acceptEdits"),
        _ => None,
    }
}

/// Whether auto mode's classifier runs on `model`, by the documented exclusions. A name this
/// list does not recognize is treated as current: new ids appear far more often than old
/// ones get pinned, and the cost of being wrong is a startup complaint rather than a
/// silently weaker posture.
fn classifier_supports(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    if m.contains("haiku") || m.contains("claude-3") {
        return false;
    }

    // A bare family alias follows the CLI's current model and is therefore supported.
    // For pinned Opus/Sonnet ids, decide positively from the documented 4.6 floor rather
    // than trying to enumerate old releases: `...-4-20250514` is 4.0, not an unknown
    // future model. Dots and dashes are both accepted spellings in user config.
    let family = ["opus", "sonnet", "fable"]
        .into_iter()
        .find(|family| m.contains(family));
    let Some(family) = family else {
        // New families are treated as current; an unsupported one produces a clear CLI
        // startup error instead of silently weakening the permission posture.
        return true;
    };
    let normalized = m.replace('.', "-");
    let suffix = normalized
        .split_once(family)
        .map(|(_, suffix)| suffix.trim_start_matches('-'))
        .unwrap_or("");
    if suffix.is_empty() {
        return true;
    }
    let numbers: Vec<u64> = suffix
        .split('-')
        .filter_map(|part| part.parse().ok())
        .collect();
    let Some(&major) = numbers.first() else {
        return true;
    };
    if family == "fable" {
        return major >= 5;
    }
    let minor = numbers.get(1).copied().filter(|n| *n < 100).unwrap_or(0);
    major > 4 || (major == 4 && minor >= 6)
}

/// How readily the workflow MANAGER escalates a decision to the supervising human via the
/// `ask_human` back-channel. **Orthogonal to [`AutonomyLevel`]**: that gates the file/tool
/// PERMISSIONS a backend is spawned with; this gates the FREQUENCY of human questions. They
/// are deliberately separate concepts — a full-autonomy manager can still be configured to
/// ask only on destructive actions (High) vs at every fork (Low).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AskTier {
    /// Ask only before destructive / irreversible actions. The conservative default.
    High,
    /// Also ask on a genuinely ambiguous requirement with materially diverging branches.
    Medium,
    /// Also ask on a genuine A/B fork with no safe default. Maps to [`AutonomyLevel::Prompted`].
    Low,
}

impl AskTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            AskTier::High => "high",
            AskTier::Medium => "medium",
            AskTier::Low => "low",
        }
    }

    /// Derive an ask tier from a backend's permission posture. Today every exec backend is
    /// `FullAutonomy` → `High`; a future `Prompted` "safe mode" maps to `Low` (ask often).
    pub fn from_autonomy(level: AutonomyLevel) -> Self {
        match level {
            AutonomyLevel::FullAutonomy => AskTier::High,
            AutonomyLevel::Prompted => AskTier::Low,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_is_the_default_posture_and_old_models_keep_accept_edits() {
        // No model and the aliases both mean "whatever is current", which is supported.
        assert_eq!(claude_permission_mode(None), "auto");
        assert_eq!(claude_permission_mode(Some("opus")), "auto");
        assert_eq!(claude_permission_mode(Some("sonnet")), "auto");
        assert_eq!(claude_permission_mode(Some("claude-opus-5")), "auto");
        assert_eq!(claude_permission_mode(Some("claude-fable-5")), "auto");
        assert_eq!(claude_permission_mode(Some("claude-sonnet-4-6")), "auto");
        assert_eq!(claude_permission_mode(Some("claude-opus-4.6")), "auto");
        // The documented exclusions, in the spellings a config file actually carries.
        for pinned in [
            "haiku",
            "claude-haiku-4-5-20251001",
            "claude-3-5-sonnet-20241022",
            "us.anthropic.claude-3-5-sonnet-20241022-v2:0",
            "anthropic.claude-3-opus-20240229-v1:0",
            "claude-sonnet-4-20250514",
            "claude-opus-4-20250514",
            "claude-opus-4-5",
            "claude-sonnet-4-5-20250929",
            "Claude-Opus-4.5",
        ] {
            assert_eq!(
                claude_permission_mode(Some(pinned)),
                "acceptEdits",
                "{pinned} has no auto-mode classifier"
            );
        }
    }

    #[test]
    fn managed_policy_override_accepts_only_safe_noninteractive_modes() {
        assert_eq!(permission_mode_override("acceptEdits"), Some("acceptEdits"));
        assert_eq!(
            permission_mode_override(" ACCEPT-EDITS "),
            Some("acceptEdits")
        );
        assert_eq!(permission_mode_override("auto"), Some("auto"));
        assert_eq!(permission_mode_override("bypassPermissions"), None);
        assert_eq!(permission_mode_override("manual"), None);
    }

    #[test]
    fn level_str_is_stable() {
        assert_eq!(AutonomyLevel::FullAutonomy.as_str(), "full-autonomy");
        assert_eq!(AutonomyLevel::Prompted.as_str(), "prompted");
    }

    #[test]
    fn ask_tier_str_is_stable() {
        assert_eq!(AskTier::High.as_str(), "high");
        assert_eq!(AskTier::Medium.as_str(), "medium");
        assert_eq!(AskTier::Low.as_str(), "low");
    }

    #[test]
    fn ask_tier_derives_from_autonomy() {
        assert_eq!(
            AskTier::from_autonomy(AutonomyLevel::FullAutonomy),
            AskTier::High
        );
        assert_eq!(
            AskTier::from_autonomy(AutonomyLevel::Prompted),
            AskTier::Low
        );
    }
}
