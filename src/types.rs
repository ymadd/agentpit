use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum BackendId {
    Claude,
    Codex,
    Gemini,
    Opencode,
    Goose,
    Copilot,
}

impl BackendId {
    pub const ALL: &'static [BackendId] = &[
        BackendId::Claude,
        BackendId::Codex,
        BackendId::Gemini,
        BackendId::Opencode,
        BackendId::Goose,
        BackendId::Copilot,
    ];

    pub fn as_str(&self) -> &'static str {
        match self {
            BackendId::Claude => "claude",
            BackendId::Codex => "codex",
            BackendId::Gemini => "gemini",
            BackendId::Opencode => "opencode",
            BackendId::Goose => "goose",
            BackendId::Copilot => "copilot",
        }
    }
}

impl fmt::Display for BackendId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for BackendId {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "claude" => Ok(BackendId::Claude),
            "codex" => Ok(BackendId::Codex),
            "gemini" => Ok(BackendId::Gemini),
            "opencode" => Ok(BackendId::Opencode),
            "goose" => Ok(BackendId::Goose),
            "copilot" => Ok(BackendId::Copilot),
            other => Err(format!("unknown backend: {other}")),
        }
    }
}

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
        assert_eq!("gemini".parse::<BackendId>().unwrap(), BackendId::Gemini);
        assert_eq!("OPENCODE".parse::<BackendId>().unwrap(), BackendId::Opencode);
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
