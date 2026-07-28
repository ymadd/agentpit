use std::path::PathBuf;

use super::{AcpAdapter, SpawnSpec};
use crate::types::BackendId;

pub struct OpencodeAdapter;

pub fn opencode_binary() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("/"))
        .join(".opencode")
        .join("bin")
        .join("opencode")
}

impl AcpAdapter for OpencodeAdapter {
    fn id(&self) -> BackendId {
        BackendId::Opencode
    }

    fn spawn_spec(&self, model: Option<&str>) -> SpawnSpec {
        let bin = opencode_binary();
        // opencode >= 1.18 rejects `--model` on the `acp` subcommand in any position (yargs
        // prints usage and exits 1), and model selection over the ACP protocol is unreliable
        // on the opencode side (anomalyco/opencode#13644). `OPENCODE_CONFIG_CONTENT` is
        // merged over the user's opencode.json for this process only, so the pin reaches
        // opencode without touching their config file (verified against opencode 1.18.8 with
        // `opencode debug config`). The ACP spawn line parses leading `NAME=value` as env.
        let env = match model {
            Some(model) => format!(
                "OPENCODE_CONFIG_CONTENT='{}' ",
                serde_json::json!({ "model": model })
            ),
            None => String::new(),
        };
        SpawnSpec {
            command_line: format!("{env}{} acp", bin.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn spawn_spec_never_passes_a_model_flag() {
        // Regression: `opencode acp --model x` exits 1 with usage on opencode >= 1.18,
        // which surfaced as "Process exited with exit status: 1" on every ACP dispatch.
        let spec = OpencodeAdapter.spawn_spec(Some("provider/model"));
        assert!(
            spec.command_line.ends_with("opencode acp"),
            "got: {}",
            spec.command_line
        );
        assert!(!spec.command_line.contains("--model"));
        let bare = OpencodeAdapter.spawn_spec(None);
        assert!(!bare.command_line.contains("OPENCODE_CONFIG_CONTENT"));
    }

    #[test]
    fn pinned_model_survives_the_spawn_line_parse() {
        // The JSON payload is single-quoted so shell_words keeps it as one argument; this
        // asserts the env var reaches the child with the model intact.
        let spec = OpencodeAdapter.spawn_spec(Some("cloudflare-ai-gateway/workers-ai/@cf/x-1"));
        let agent = agent_client_protocol::AcpAgent::from_str(&spec.command_line).unwrap();
        let parsed = format!("{agent:?}");
        assert!(
            parsed.contains(
                r#"OPENCODE_CONFIG_CONTENT", value: "{\"model\":\"cloudflare-ai-gateway/workers-ai/@cf/x-1\"}"#
            ),
            "got: {parsed}"
        );
    }
}
