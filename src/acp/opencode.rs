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

    fn spawn_spec(&self) -> SpawnSpec {
        let bin = opencode_binary();
        SpawnSpec {
            command_line: format!("{} acp", bin.display()),
        }
    }
}
