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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_str_is_stable() {
        assert_eq!(AutonomyLevel::FullAutonomy.as_str(), "full-autonomy");
        assert_eq!(AutonomyLevel::Prompted.as_str(), "prompted");
    }
}
