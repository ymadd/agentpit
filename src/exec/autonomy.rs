//! Central declaration of how much autonomy each exec backend is granted.
//!
//! Every exec backend in agentpit runs **non-interactively** — there is no human at a TTY
//! to answer permission prompts — so each CLI is launched with whatever flag makes it skip
//! its approval gates: `agy --dangerously-skip-permissions`, `gemini --yolo --skip-trust`,
//! `claude --permission-mode acceptEdits`, or, for `codex exec`, the inherently
//! non-interactive `exec` subcommand. That posture is a real security decision — the
//! backend may read, edit, and run tools in `cwd` without confirmation.
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
        assert_eq!(AskTier::from_autonomy(AutonomyLevel::FullAutonomy), AskTier::High);
        assert_eq!(AskTier::from_autonomy(AutonomyLevel::Prompted), AskTier::Low);
    }
}
