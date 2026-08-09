//! The prompt cache: what `mcp refresh` wrote down, and what every other surface reads.
//!
//! Listing an MCP server's prompts costs a process spawn and a JSON-RPC handshake. A slash
//! surface has to answer "what commands exist?" synchronously, at startup, for every
//! configured server — so it does not ask the servers. It reads this file, which one
//! explicit `agentpit mcp refresh` filled in.
//!
//! That split is the reason for the staleness key. A cached entry records the
//! [`ServerDef::cache_key`] it was fetched under; edit the command, its arguments, its
//! environment, or move the definition to another file, and the key no longer matches, so
//! the entry stops being offered and `mcp list` says why. Nothing expires on a clock: a
//! server whose definition has not changed keeps its prompts until the user asks for new
//! ones. Time is recorded for display only.
//!
//! One file for the machine, under the state directory, so a refresh in one checkout is not
//! lost by working in another. Entries are keyed by server name; the staleness key is what
//! keeps two projects' same-named servers from being confused for each other.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::servers::ServerDef;

/// Bumped when the on-disk shape changes incompatibly. A file from another version is
/// discarded (not migrated, not an error): its whole content is re-derivable by a refresh.
const CACHE_VERSION: u32 = 1;

pub fn cache_path() -> PathBuf {
    agentpit_events::state_dir()
        .join("mcp")
        .join("prompts.json")
}

/// One prompt as its server advertised it in `prompts/list`.
///
/// The description and argument names are the whole payload: the prompt's *body* lives on
/// the server and is not cached, which is why an invocation composes a turn that names the
/// prompt rather than pasting it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedPrompt {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub arguments: Vec<CachedArgument>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedArgument {
    pub name: String,
    #[serde(default)]
    pub required: bool,
}

/// What one server advertised, and the definition it was fetched under.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedServer {
    /// [`ServerDef::cache_key`] at fetch time.
    pub key: String,
    /// Unix seconds, for display only — nothing expires on a clock.
    #[serde(default)]
    pub refreshed_at: u64,
    #[serde(default)]
    pub prompts: Vec<CachedPrompt>,
}

impl CachedServer {
    /// Does this entry still describe `def`?
    pub fn matches(&self, def: &ServerDef) -> bool {
        self.key == def.cache_key()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PromptCache {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub servers: BTreeMap<String, CachedServer>,
}

impl PromptCache {
    /// The cache as it is on disk, or an empty one.
    ///
    /// Never an error: this runs on the startup path of every interactive surface, and a
    /// corrupt cache must cost the user their MCP commands, not their session. The next
    /// refresh rewrites it.
    pub fn load() -> PromptCache {
        Self::load_from(&cache_path())
    }

    pub fn load_from(path: &std::path::Path) -> PromptCache {
        let Ok(raw) = std::fs::read_to_string(path) else {
            return PromptCache::default();
        };
        match serde_json::from_str::<PromptCache>(&raw) {
            Ok(cache) if cache.version == CACHE_VERSION => cache,
            // Wrong version or unparseable: start over rather than guess.
            _ => PromptCache::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&cache_path())
    }

    pub fn save_to(&self, path: &std::path::Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let mut out = self.clone();
        out.version = CACHE_VERSION;
        let raw = serde_json::to_string_pretty(&out)?;
        std::fs::write(path, raw).with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    /// The cached prompts for `def`, or `None` when there is no entry or the entry was
    /// fetched under a different definition.
    pub fn fresh_for(&self, def: &ServerDef) -> Option<&CachedServer> {
        self.servers.get(&def.name).filter(|e| e.matches(def))
    }

    /// Record a fetch. Replaces any previous entry for the name outright — a prompt the
    /// server has stopped advertising must disappear.
    pub fn put(&mut self, def: &ServerDef, prompts: Vec<CachedPrompt>) {
        self.servers.insert(
            def.name.clone(),
            CachedServer {
                key: def.cache_key(),
                refreshed_at: now_secs(),
                prompts,
            },
        );
    }
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// "3m ago" / "2d ago" — how the age of an entry is shown. Coarse on purpose: what matters
/// is "recent" vs "old", never the second.
pub fn age_label(refreshed_at: u64) -> String {
    let now = now_secs();
    if refreshed_at == 0 || refreshed_at > now {
        return "unknown".to_string();
    }
    let secs = now - refreshed_at;
    match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs / 60),
        3600..=86399 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mcp::servers::{Origin, ServerDef};
    use tempfile::tempdir;

    fn def(command: &str) -> ServerDef {
        ServerDef {
            name: "s".into(),
            command: command.into(),
            args: vec![],
            env: BTreeMap::new(),
            cwd: String::new(),
            enabled: true,
            origin: Origin::Config,
        }
    }

    fn prompt(name: &str) -> CachedPrompt {
        CachedPrompt {
            name: name.into(),
            description: "does a thing".into(),
            arguments: vec![CachedArgument {
                name: "topic".into(),
                required: true,
            }],
        }
    }

    #[test]
    fn round_trips_through_the_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("mcp").join("prompts.json");
        let mut cache = PromptCache::default();
        cache.put(&def("npx"), vec![prompt("docs")]);
        cache.save_to(&path).unwrap();

        let loaded = PromptCache::load_from(&path);
        assert_eq!(loaded.version, CACHE_VERSION);
        assert_eq!(loaded.servers["s"].prompts, vec![prompt("docs")]);
        assert_eq!(loaded.servers, cache.servers);
    }

    #[test]
    fn staleness_key_follows_the_definition() {
        let mut cache = PromptCache::default();
        cache.put(&def("npx"), vec![prompt("docs")]);

        // Same definition: the entry is offered.
        assert!(cache.fresh_for(&def("npx")).is_some());
        // The command changed: what was cached describes a different server now.
        assert!(cache.fresh_for(&def("other-binary")).is_none());
        // An unknown name has no entry at all.
        let mut renamed = def("npx");
        renamed.name = "elsewhere".into();
        assert!(cache.fresh_for(&renamed).is_none());
    }

    #[test]
    fn a_refresh_drops_prompts_the_server_stopped_advertising() {
        let mut cache = PromptCache::default();
        cache.put(&def("npx"), vec![prompt("docs"), prompt("gone")]);
        cache.put(&def("npx"), vec![prompt("docs")]);
        let entry = cache.fresh_for(&def("npx")).unwrap();
        assert_eq!(entry.prompts.len(), 1);
        assert_eq!(entry.prompts[0].name, "docs");
    }

    #[test]
    fn a_missing_or_corrupt_cache_is_empty_never_an_error() {
        let dir = tempdir().unwrap();
        assert_eq!(
            PromptCache::load_from(&dir.path().join("absent.json")),
            PromptCache::default()
        );

        let junk = dir.path().join("junk.json");
        std::fs::write(&junk, "{{{ not json").unwrap();
        assert_eq!(PromptCache::load_from(&junk), PromptCache::default());

        // A file from a future/foreign version is discarded rather than half-read.
        let future = dir.path().join("future.json");
        std::fs::write(
            &future,
            r#"{"version": 99, "servers": {"s": {"key": "k"}}}"#,
        )
        .unwrap();
        assert_eq!(PromptCache::load_from(&future), PromptCache::default());
    }

    #[test]
    fn age_label_is_coarse_and_never_panics() {
        let now = now_secs();
        assert_eq!(age_label(now), "just now");
        assert_eq!(age_label(now - 120), "2m ago");
        assert_eq!(age_label(now - 7200), "2h ago");
        assert_eq!(age_label(now - 3 * 86400), "3d ago");
        assert_eq!(age_label(0), "unknown");
        // A clock that moved backwards is not a crash.
        assert_eq!(age_label(now + 10_000), "unknown");
    }
}
