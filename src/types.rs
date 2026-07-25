use std::fmt;

use serde::{Deserialize, Serialize};

// BackendId is defined in the shared `agentpit-events` crate so the event schema and the
// CLI agree on one definition. Re-exported here so the CLI keeps using `crate::types::BackendId`.
pub use agentpit_events::BackendId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Exec,
    Acp,
}

impl Transport {
    pub fn as_str(&self) -> &'static str {
        match self {
            Transport::Exec => "exec",
            Transport::Acp => "acp",
        }
    }
}

impl fmt::Display for Transport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_backends() {
        assert_eq!(
            "opencode".parse::<BackendId>().unwrap(),
            BackendId::Opencode
        );
        assert_eq!(
            "OPENCODE".parse::<BackendId>().unwrap(),
            BackendId::Opencode
        );
    }

    #[test]
    fn rejects_unknown_backends() {
        assert!("ghost".parse::<BackendId>().is_err());
    }

    #[test]
    fn display_round_trips() {
        for id in BackendId::ALL {
            assert_eq!(id.as_str().parse::<BackendId>().unwrap(), *id);
        }
    }
}
