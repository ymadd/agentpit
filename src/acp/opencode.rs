use std::path::PathBuf;

use super::{AcpAdapter, SpawnSpec};
use crate::effort::Effort;
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

    fn spawn_spec(&self, model: Option<&str>, effort: Option<Effort>) -> SpawnSpec {
        let bin = opencode_binary();
        let env = match config_content(model, effort) {
            Some(json) => format!("OPENCODE_CONFIG_CONTENT='{json}' "),
            None => String::new(),
        };
        SpawnSpec {
            command_line: format!("{env}{} acp", bin.display()),
        }
    }
}

/// The `OPENCODE_CONFIG_CONTENT` payload pinning `model` / `effort`, or `None` when neither is
/// set (no env prefix at all — byte-identical to an unpinned spawn).
///
/// opencode >= 1.18 rejects `--model` on the `acp` subcommand in any position (yargs prints usage
/// and exits 1), and model selection over the ACP protocol is unreliable on the opencode side
/// (anomalyco/opencode#13644). `OPENCODE_CONFIG_CONTENT` is merged over the user's opencode.json
/// for this process only, so both pins reach opencode without touching their config file (verified
/// against opencode 1.18.8/1.18.9 with `opencode debug config`). The ACP spawn line parses a
/// leading `NAME=value` as env.
///
/// Effort has no top-level config key: opencode expresses it as a model *variant*, which lives on
/// an agent (`agent.<name>.variant`). `build` is opencode's default agent, so that is where the
/// variant goes. Its schema notes a variant "applies only when using the agent's configured
/// model", so when a model is pinned it is set on the agent as well as at the top level.
fn config_content(model: Option<&str>, effort: Option<Effort>) -> Option<String> {
    if model.is_none() && effort.is_none() {
        return None;
    }
    let mut config = serde_json::Map::new();
    if let Some(m) = model {
        config.insert("model".into(), m.into());
    }
    if let Some(e) = effort {
        let mut agent = serde_json::Map::new();
        // Pass the rung through unclamped: `--variant` documents itself as "provider-specific
        // reasoning effort", so the provider — not agentpit — decides which rungs it knows.
        agent.insert(
            "variant".into(),
            e.clamp_for(BackendId::Opencode).as_str().into(),
        );
        if let Some(m) = model {
            agent.insert("model".into(), m.into());
        }
        config.insert("agent".into(), serde_json::json!({ "build": agent }));
    }
    Some(serde_json::Value::Object(config).to_string())
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    #[test]
    fn spawn_spec_never_passes_a_model_flag() {
        // Regression: `opencode acp --model x` exits 1 with usage on opencode >= 1.18,
        // which surfaced as "Process exited with exit status: 1" on every ACP dispatch.
        let spec = OpencodeAdapter.spawn_spec(Some("provider/model"), None);
        assert!(
            spec.command_line.ends_with("opencode acp"),
            "got: {}",
            spec.command_line
        );
        assert!(!spec.command_line.contains("--model"));
        let bare = OpencodeAdapter.spawn_spec(None, None);
        assert!(!bare.command_line.contains("OPENCODE_CONFIG_CONTENT"));
        // Neither pin set → no env prefix at all, not an empty JSON object.
        assert!(!bare.command_line.contains("variant"));
    }

    #[test]
    fn effort_becomes_the_build_agents_variant() {
        // Effort alone: only the agent block is emitted, no model key.
        let json = config_content(None, Some(Effort::High)).unwrap();
        assert_eq!(json, r#"{"agent":{"build":{"variant":"high"}}}"#);
        // With a model, the agent carries it too — opencode applies a variant only when the
        // agent has a configured model.
        let both = config_content(Some("openai/gpt-5.4"), Some(Effort::Max)).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&both).unwrap();
        assert_eq!(parsed["model"], "openai/gpt-5.4");
        assert_eq!(parsed["agent"]["build"]["variant"], "max");
        assert_eq!(parsed["agent"]["build"]["model"], "openai/gpt-5.4");
    }

    #[test]
    fn pinned_model_survives_the_spawn_line_parse() {
        // The JSON payload is single-quoted so shell_words keeps it as one argument; this
        // asserts the env var reaches the child with the model intact.
        let spec =
            OpencodeAdapter.spawn_spec(Some("cloudflare-ai-gateway/workers-ai/@cf/x-1"), None);
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
