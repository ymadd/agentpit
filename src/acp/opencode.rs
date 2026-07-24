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
        // prints usage and exits 1), so passing it broke the ACP transport outright. Model
        // selection over the ACP protocol is likewise unreliable on the opencode side
        // (anomalyco/opencode#13644), so spawn plain and let opencode's own config
        // (`"model"` in opencode.json) pick the model — with a warning so a pinned model
        // doesn't silently mean something else.
        if let Some(model) = model {
            eprintln!(
                "[opencode] ignoring model `{model}`: `opencode acp` no longer accepts --model; \
                 set \"model\" in ~/.config/opencode/opencode.json instead"
            );
        }
        SpawnSpec {
            command_line: format!("{} acp", bin.display()),
        }
    }
}

#[cfg(test)]
mod tests {
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
        assert_eq!(bare.command_line, spec.command_line);
    }
}
