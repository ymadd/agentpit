//! `TaskCategory` — the diagnosis output unit and the columns of a capability profile.
//!
//! Orthogonal to `RouteKey` (which is command-shaped): a single command can diagnose
//! into any of these categories. `LongContext` is really a feature, not a category — it
//! is only promoted to a category when the long-context threshold is exceeded.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// What kind of work a task is, used to score backends and route diagnostically.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum TaskCategory {
    Coding,
    Refactor,
    Review,
    AdversarialReview,
    SecurityReview,
    Debug,
    Explain,
    Docs,
    Planning,
    LongContext,
}

impl TaskCategory {
    /// Every category, in a stable declaration order (used to seed/iterate profiles).
    pub const ALL: &'static [TaskCategory] = &[
        TaskCategory::Coding,
        TaskCategory::Refactor,
        TaskCategory::Review,
        TaskCategory::AdversarialReview,
        TaskCategory::SecurityReview,
        TaskCategory::Debug,
        TaskCategory::Explain,
        TaskCategory::Docs,
        TaskCategory::Planning,
        TaskCategory::LongContext,
    ];

    /// Canonical lowercase token. Matches the `serde(rename_all = "lowercase")` wire form
    /// so serialization, `Display`, and `FromStr` all agree.
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskCategory::Coding => "coding",
            TaskCategory::Refactor => "refactor",
            TaskCategory::Review => "review",
            TaskCategory::AdversarialReview => "adversarialreview",
            TaskCategory::SecurityReview => "securityreview",
            TaskCategory::Debug => "debug",
            TaskCategory::Explain => "explain",
            TaskCategory::Docs => "docs",
            TaskCategory::Planning => "planning",
            TaskCategory::LongContext => "longcontext",
        }
    }
}

impl fmt::Display for TaskCategory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for TaskCategory {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Accept the canonical lowercase form plus snake_case / kebab-case aliases for the
        // multi-word variants, so CLI input stays ergonomic. `Display` always emits the
        // canonical form, so `Display -> FromStr` round-trips.
        let normalized = s.trim().to_ascii_lowercase().replace(['_', '-', ' '], "");
        match normalized.as_str() {
            "coding" => Ok(TaskCategory::Coding),
            "refactor" => Ok(TaskCategory::Refactor),
            "review" => Ok(TaskCategory::Review),
            "adversarialreview" => Ok(TaskCategory::AdversarialReview),
            "securityreview" => Ok(TaskCategory::SecurityReview),
            "debug" => Ok(TaskCategory::Debug),
            "explain" => Ok(TaskCategory::Explain),
            "docs" => Ok(TaskCategory::Docs),
            "planning" => Ok(TaskCategory::Planning),
            "longcontext" => Ok(TaskCategory::LongContext),
            other => Err(format!("unknown task category: {other}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_from_str_round_trips() {
        for cat in TaskCategory::ALL {
            assert_eq!(cat.to_string().parse::<TaskCategory>().unwrap(), *cat);
            assert_eq!(cat.as_str().parse::<TaskCategory>().unwrap(), *cat);
        }
    }

    #[test]
    fn from_str_accepts_aliases() {
        assert_eq!(
            "adversarial_review".parse::<TaskCategory>().unwrap(),
            TaskCategory::AdversarialReview
        );
        assert_eq!(
            "security-review".parse::<TaskCategory>().unwrap(),
            TaskCategory::SecurityReview
        );
        assert_eq!(
            "Long Context".parse::<TaskCategory>().unwrap(),
            TaskCategory::LongContext
        );
    }

    #[test]
    fn from_str_rejects_unknown() {
        assert!("ghost".parse::<TaskCategory>().is_err());
    }

    #[test]
    fn serde_uses_lowercase_tokens() {
        let json = serde_json::to_string(&TaskCategory::AdversarialReview).unwrap();
        assert_eq!(json, "\"adversarialreview\"");
        let back: TaskCategory = serde_json::from_str("\"longcontext\"").unwrap();
        assert_eq!(back, TaskCategory::LongContext);
    }
}
