//! Central declaration of the reasoning-effort ladder every backend is dispatched at.
//!
//! Each backend CLI exposes reasoning effort under its own name and its own vocabulary
//! (`claude --effort`, `codex -c model_reasoning_effort=`, `agy --effort`, opencode's model
//! *variant*), and the rungs do not line up: agy stops at `high`, codex's top rung is `xhigh`,
//! claude goes one further to `max`. Rather than leave that mismatch to each adapter's flag
//! building — the same reasoning [`autonomy`](crate::exec::autonomy) centralises for permission
//! posture — agentpit defines ONE canonical ladder here and declares, in a single auditable
//! table, what each rung becomes on each backend.
//!
//! A rung a backend cannot express is **clamped down**, never up: asking for more thinking than
//! the CLI offers gets its maximum, and never silently gets less than the next rung down. The
//! clamped value is what dispatch records, so telemetry and benchmark results carry the effort
//! that actually ran rather than the one that was asked for.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::types::BackendId;

/// A rung on the canonical reasoning-effort ladder, ordered low → high. `None` anywhere an
/// `Option<Effort>` is threaded means "leave the CLI on its own default" (no flag emitted).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl Effort {
    pub const ALL: &'static [Effort] = &[
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::XHigh,
        Effort::Max,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::XHigh => "xhigh",
            Effort::Max => "max",
        }
    }

    /// The rung `self` becomes on `backend`, clamped to the highest rung that backend's CLI
    /// accepts. Verified against the locally installed CLIs (2026-07-31):
    ///
    /// | canonical | claude 2.1 `--effort` | codex 0.146 `model_reasoning_effort` | agy 1.1 `--effort` | opencode 1.18 variant |
    /// |---|---|---|---|---|
    /// | low / medium / high | same | same | same | same |
    /// | xhigh | `xhigh` | `xhigh` | **`high`** | `xhigh` |
    /// | max | `max` | **`xhigh`** | **`high`** | `max` |
    ///
    /// opencode's variant is provider-specific by definition (`--variant` documents itself as
    /// "provider-specific reasoning effort"), so the rung passes through unclamped and the
    /// provider resolves it.
    ///
    /// Clamping is not the whole story for agy: its model ids BUNDLE the level
    /// (`gemini-3.6-flash-high`), and it rejects a separate `--effort` for such a model, so a
    /// pinned model suppresses the flag entirely — see
    /// [`AntigravityExec::build_spec`](crate::exec::antigravity::AntigravityExec).
    pub fn clamp_for(self, backend: BackendId) -> Effort {
        let ceiling = match backend {
            // agy's own help enumerates exactly (low|medium|high).
            BackendId::Antigravity => Effort::High,
            // codex tops out at xhigh (and only on models that offer it).
            BackendId::Codex => Effort::XHigh,
            BackendId::Claude | BackendId::Opencode | BackendId::Goose | BackendId::Copilot => {
                Effort::Max
            }
        };
        self.min(ceiling)
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Effort {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" => Ok(Effort::Low),
            "medium" | "med" => Ok(Effort::Medium),
            "high" => Ok(Effort::High),
            "xhigh" => Ok(Effort::XHigh),
            "max" => Ok(Effort::Max),
            other => Err(format!(
                "unknown effort '{other}' (expected one of: low, medium, high, xhigh, max)"
            )),
        }
    }
}

/// Resolve the effective effort for a dispatch by precedence: an explicit `--effort` wins, then
/// the role's `effort`, then the backend's `[backends.<id>].effort` default; `None` = the CLI's
/// own default (no flag emitted). Mirrors [`resolve_model`](crate::workflow::roles::resolve_model)
/// so model and effort follow one precedence rule, not two.
pub fn resolve_effort(
    explicit: Option<Effort>,
    role_effort: Option<Effort>,
    backend_default: Option<Effort>,
) -> Option<Effort> {
    explicit.or(role_effort).or(backend_default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_round_trips_every_rung() {
        for e in Effort::ALL {
            assert_eq!(e.as_str().parse::<Effort>().unwrap(), *e);
        }
        assert_eq!("HIGH".parse::<Effort>().unwrap(), Effort::High);
        assert_eq!(" med ".parse::<Effort>().unwrap(), Effort::Medium);
        assert!("turbo".parse::<Effort>().is_err());
    }

    #[test]
    fn ladder_is_ordered_low_to_high() {
        assert!(Effort::Low < Effort::Medium);
        assert!(Effort::Medium < Effort::High);
        assert!(Effort::High < Effort::XHigh);
        assert!(Effort::XHigh < Effort::Max);
    }

    #[test]
    fn clamps_down_to_each_backend_ceiling_never_up() {
        // agy stops at high.
        assert_eq!(Effort::Max.clamp_for(BackendId::Antigravity), Effort::High);
        assert_eq!(
            Effort::XHigh.clamp_for(BackendId::Antigravity),
            Effort::High
        );
        assert_eq!(Effort::Low.clamp_for(BackendId::Antigravity), Effort::Low);
        // codex stops at xhigh.
        assert_eq!(Effort::Max.clamp_for(BackendId::Codex), Effort::XHigh);
        assert_eq!(Effort::XHigh.clamp_for(BackendId::Codex), Effort::XHigh);
        // claude carries the whole ladder.
        assert_eq!(Effort::Max.clamp_for(BackendId::Claude), Effort::Max);
        // Clamping never raises a rung.
        for backend in BackendId::ALL {
            for e in Effort::ALL {
                assert!(e.clamp_for(*backend) <= *e);
            }
        }
    }

    #[test]
    fn precedence_is_explicit_then_role_then_backend_default() {
        assert_eq!(
            resolve_effort(Some(Effort::Low), Some(Effort::High), Some(Effort::Max)),
            Some(Effort::Low)
        );
        assert_eq!(
            resolve_effort(None, Some(Effort::High), Some(Effort::Max)),
            Some(Effort::High)
        );
        assert_eq!(
            resolve_effort(None, None, Some(Effort::Max)),
            Some(Effort::Max)
        );
        assert_eq!(resolve_effort(None, None, None), None);
    }
}
