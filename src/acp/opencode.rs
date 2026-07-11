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
        // opencode's global `--model <provider/model>` flag applies to the `acp` subcommand too.
        // Model ids carry no spaces, so appending is safe for the command-line parser.
        let model_flag = model.map(|m| format!(" --model {m}")).unwrap_or_default();
        SpawnSpec {
            command_line: format!("{} acp{}", bin.display(), model_flag),
        }
    }
}
