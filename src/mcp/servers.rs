//! Where an MCP server *definition* comes from, and what one is.
//!
//! Three files can name a server, and they are not peers:
//!
//! ```text
//! ~/.config/agentpit/config.toml   [mcp.servers.<name>]   ← agentpit's own, authoritative
//! <cwd>/.mcp.json                  mcpServers.<name>      ← the project's, additive
//! ~/.claude.json                   mcpServers.<name>      ← import SOURCE only, never read live
//! ```
//!
//! The first two are read on every surface that needs the list. The third is *not*: it
//! belongs to another tool, and silently inheriting another tool's spawn list is not a thing
//! agentpit does. [`crate::mcp::import`] copies out of it once, on request, with a diff.
//!
//! A definition is a recipe, never a running process. Reading these files spawns nothing;
//! [`crate::mcp::client`] is the only module that starts a child, and only `mcp refresh`
//! calls it.
//!
//! ## What is rejected, and how
//!
//! Both readers take input agentpit did not write, so neither may panic and neither may let
//! one bad entry cost the user the good ones. A malformed `.mcp.json` yields no servers; an
//! entry inside a well-formed one that is not a runnable stdio server (no `command`, or a
//! `url`/`type` naming a transport this module cannot speak) is skipped with a reason, and
//! the reasons are shown by `mcp list`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::config::{HubConfig, McpServerConfig};

/// The project-scope file, relative to the session's working directory.
pub const PROJECT_FILE: &str = ".mcp.json";

/// A `.mcp.json` larger than this is not read. It is a hand-written list of a handful of
/// servers; anything this size is not that, and the whole file is parsed into memory.
const MAX_PROJECT_FILE_BYTES: u64 = 1024 * 1024;

/// Which file a definition came from — carried so `mcp list` can say, and so the cache key
/// can tell two different servers that share a name apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// `[mcp.servers.<name>]` in agentpit's own config.
    Config,
    /// `mcpServers.<name>` in a project's `.mcp.json`.
    Project(PathBuf),
}

impl Origin {
    pub fn label(&self) -> String {
        match self {
            Origin::Config => "config".to_string(),
            Origin::Project(path) => format!("{}", path.display()),
        }
    }
}

/// One stdio MCP server, resolved from whichever file named it.
#[derive(Debug, Clone, PartialEq)]
pub struct ServerDef {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// Empty = inherit agentpit's working directory.
    pub cwd: String,
    pub enabled: bool,
    pub origin: Origin,
}

impl ServerDef {
    /// The staleness key for this definition: change what agentpit would *run* and the
    /// cached prompt list stops applying.
    ///
    /// Hashed rather than stored plainly because `env` routinely holds API keys, and the
    /// cache file is a plain-text file in the state directory. The hash still covers the
    /// values — a server whose behaviour is switched by an env var must go stale — it just
    /// does not carry them.
    pub fn cache_key(&self) -> String {
        let mut hasher = Sha256::new();
        // Length-prefixed so no concatenation of fields can collide with another.
        let mut field = |s: &str| {
            hasher.update((s.len() as u64).to_le_bytes());
            hasher.update(s.as_bytes());
        };
        field(&self.name);
        field(&self.command);
        for arg in &self.args {
            field(arg);
        }
        for (k, v) in &self.env {
            field(k);
            field(v);
        }
        field(&self.cwd);
        field(&self.origin.label());
        format!("{:x}", hasher.finalize())
    }
}

/// An entry that named itself a server but cannot be run as one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejected {
    pub name: String,
    pub reason: RejectReason,
    pub source: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// No `command`, or an empty one: nothing to spawn.
    NoCommand,
    /// `type`/`url` names a transport this module does not speak (sse, http, …).
    NotStdio,
    /// The file itself is not readable / not JSON / not an object of servers.
    Unreadable,
    /// The file is larger than [`MAX_PROJECT_FILE_BYTES`].
    TooLarge,
}

impl RejectReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            RejectReason::NoCommand => "no command",
            RejectReason::NotStdio => "not a stdio server",
            RejectReason::Unreadable => "unreadable",
            RejectReason::TooLarge => "too large",
        }
    }
}

/// The result of gathering definitions: what can be run, and what was refused.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Servers {
    /// Runnable definitions, config scope first, in name order within a scope.
    pub defs: Vec<ServerDef>,
    pub rejected: Vec<Rejected>,
}

impl Servers {
    pub fn get(&self, name: &str) -> Option<&ServerDef> {
        self.defs.iter().find(|d| d.name == name)
    }
}

/// Every server agentpit would offer for `cwd`: the config's, then the project file's.
///
/// A name defined in both belongs to the config — agentpit's own file is the one the user
/// controls directly, and a checked-in `.mcp.json` must not be able to redefine what a
/// `/<server>:<prompt>` command runs.
pub fn gather(config: &HubConfig, cwd: &Path) -> Servers {
    let mut servers = Servers::default();
    for (name, cfg) in &config.mcp.servers {
        match from_config(name, cfg) {
            Ok(def) => servers.defs.push(def),
            Err(reason) => servers.rejected.push(Rejected {
                name: name.clone(),
                reason,
                source: crate::config::default_config_path(),
            }),
        }
    }
    let project = cwd.join(PROJECT_FILE);
    let found = read_project_file(&project);
    for def in found.defs {
        if !servers.defs.iter().any(|d| d.name == def.name) {
            servers.defs.push(def);
        }
    }
    servers.rejected.extend(found.rejected);
    servers
}

fn from_config(name: &str, cfg: &McpServerConfig) -> Result<ServerDef, RejectReason> {
    if cfg.command.trim().is_empty() {
        return Err(RejectReason::NoCommand);
    }
    Ok(ServerDef {
        name: name.to_string(),
        command: cfg.command.clone(),
        args: cfg.args.clone(),
        env: cfg.env.clone(),
        cwd: cfg.cwd.clone(),
        enabled: cfg.enabled,
        origin: Origin::Config,
    })
}

/// The wire shape of a `.mcp.json` / the `mcpServers` block of `~/.claude.json`.
///
/// Every field past the name is optional and unknown ones are ignored: this file is written
/// by other tools, and a key agentpit has never heard of is not a reason to drop a server it
/// otherwise understands.
#[derive(Debug, Deserialize)]
pub struct McpServersFile {
    #[serde(default, rename = "mcpServers")]
    pub mcp_servers: BTreeMap<String, JsonServer>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct JsonServer {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// `"stdio"` (or absent) is the only value this module can run.
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    /// Present on remote servers. Its presence alone is enough to refuse.
    #[serde(default)]
    pub url: Option<String>,
}

impl JsonServer {
    /// Turn one JSON entry into a runnable definition, or say why not.
    pub fn to_def(&self, name: &str, origin: Origin) -> Result<ServerDef, RejectReason> {
        if self.url.is_some() {
            return Err(RejectReason::NotStdio);
        }
        if let Some(kind) = &self.kind
            && !kind.eq_ignore_ascii_case("stdio")
        {
            return Err(RejectReason::NotStdio);
        }
        let command = self.command.clone().unwrap_or_default();
        if command.trim().is_empty() {
            return Err(RejectReason::NoCommand);
        }
        Ok(ServerDef {
            name: name.to_string(),
            command,
            args: self.args.clone(),
            env: self.env.clone(),
            cwd: self.cwd.clone().unwrap_or_default(),
            enabled: true,
            origin,
        })
    }
}

/// Read one project `.mcp.json`. An absent file is not an error — most projects have none.
pub fn read_project_file(path: &Path) -> Servers {
    let mut servers = Servers::default();
    let raw = match std::fs::metadata(path) {
        Ok(meta) if meta.len() > MAX_PROJECT_FILE_BYTES => {
            servers.rejected.push(Rejected {
                name: PROJECT_FILE.to_string(),
                reason: RejectReason::TooLarge,
                source: path.to_path_buf(),
            });
            return servers;
        }
        Ok(_) => match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(_) => {
                servers.rejected.push(Rejected {
                    name: PROJECT_FILE.to_string(),
                    reason: RejectReason::Unreadable,
                    source: path.to_path_buf(),
                });
                return servers;
            }
        },
        // Not there: nothing to say.
        Err(_) => return servers,
    };
    servers.extend_from_json(&raw, path, Origin::Project(path.to_path_buf()));
    servers
}

impl Servers {
    /// Parse a `{"mcpServers": {...}}` document into this set. Shared by the project reader
    /// and the `~/.claude.json` importer, which read the same shape out of different files.
    pub fn extend_from_json(&mut self, raw: &str, source: &Path, origin: Origin) {
        let parsed: McpServersFile = match serde_json::from_str(raw) {
            Ok(parsed) => parsed,
            Err(_) => {
                self.rejected.push(Rejected {
                    name: source
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| source.display().to_string()),
                    reason: RejectReason::Unreadable,
                    source: source.to_path_buf(),
                });
                return;
            }
        };
        for (name, entry) in parsed.mcp_servers {
            match entry.to_def(&name, origin.clone()) {
                Ok(def) => self.defs.push(def),
                Err(reason) => self.rejected.push(Rejected {
                    name,
                    reason,
                    source: source.to_path_buf(),
                }),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn config_with(servers: &[(&str, McpServerConfig)]) -> HubConfig {
        let mut config = HubConfig::default();
        for (name, cfg) in servers {
            config.mcp.servers.insert((*name).to_string(), cfg.clone());
        }
        config
    }

    fn cfg(command: &str) -> McpServerConfig {
        McpServerConfig {
            command: command.to_string(),
            enabled: true,
            ..Default::default()
        }
    }

    #[test]
    fn documented_project_file_shape_is_accepted() {
        let dir = tempdir().unwrap();
        let path = dir.path().join(PROJECT_FILE);
        fs::write(
            &path,
            r#"{
              "mcpServers": {
                "context7": {
                  "command": "npx",
                  "args": ["-y", "@upstash/context7-mcp"],
                  "env": { "CONTEXT7_API_KEY": "k" }
                },
                "explicit-stdio": { "type": "stdio", "command": "./server" }
              }
            }"#,
        )
        .unwrap();
        let found = read_project_file(&path);
        assert!(found.rejected.is_empty(), "{:?}", found.rejected);
        let c7 = found.get("context7").expect("context7 present");
        assert_eq!(c7.command, "npx");
        assert_eq!(c7.args, vec!["-y", "@upstash/context7-mcp"]);
        assert_eq!(
            c7.env.get("CONTEXT7_API_KEY").map(String::as_str),
            Some("k")
        );
        assert_eq!(c7.origin, Origin::Project(path.clone()));
        assert!(found.get("explicit-stdio").is_some());
    }

    /// Junk in, no panic and no half-server out. Each of these is something a real
    /// `.mcp.json` in the wild contains.
    #[test]
    fn junk_is_rejected_with_a_reason_and_never_panics() {
        let dir = tempdir().unwrap();

        let broken = dir.path().join("broken.json");
        fs::write(&broken, "{not json at all").unwrap();
        let found = read_project_file(&broken);
        assert!(found.defs.is_empty());
        assert_eq!(found.rejected[0].reason, RejectReason::Unreadable);

        // Well-formed JSON that is not a server map at all.
        let wrong = dir.path().join("wrong.json");
        fs::write(&wrong, r#"["a", "b"]"#).unwrap();
        assert_eq!(
            read_project_file(&wrong).rejected[0].reason,
            RejectReason::Unreadable
        );

        // An object with no mcpServers key is simply empty, not an error.
        let empty = dir.path().join("empty.json");
        fs::write(&empty, r#"{"other": 1}"#).unwrap();
        let found = read_project_file(&empty);
        assert!(found.defs.is_empty() && found.rejected.is_empty());

        // Per-entry refusals: the good neighbour still survives.
        let mixed = dir.path().join("mixed.json");
        fs::write(
            &mixed,
            r#"{"mcpServers": {
                 "remote": {"type": "sse", "url": "https://example.invalid/sse"},
                 "http-remote": {"url": "https://example.invalid/mcp"},
                 "nameless": {"args": ["x"]},
                 "blank": {"command": "   "},
                 "good": {"command": "./ok", "unknown_key": true}
               }}"#,
        )
        .unwrap();
        let found = read_project_file(&mixed);
        assert_eq!(found.defs.len(), 1);
        assert_eq!(found.defs[0].name, "good");
        let reasons: BTreeMap<_, _> = found
            .rejected
            .iter()
            .map(|r| (r.name.as_str(), r.reason))
            .collect();
        assert_eq!(reasons["remote"], RejectReason::NotStdio);
        assert_eq!(reasons["http-remote"], RejectReason::NotStdio);
        assert_eq!(reasons["nameless"], RejectReason::NoCommand);
        assert_eq!(reasons["blank"], RejectReason::NoCommand);

        // A file that is not there says nothing at all.
        let absent = read_project_file(&dir.path().join("nope.json"));
        assert!(absent.defs.is_empty() && absent.rejected.is_empty());
    }

    #[test]
    fn config_scope_wins_a_shared_name_and_the_project_file_only_adds() {
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join(PROJECT_FILE),
            r#"{"mcpServers": {
                 "shared": {"command": "from-project"},
                 "project-only": {"command": "only-here"}
               }}"#,
        )
        .unwrap();
        let config = config_with(&[("shared", cfg("from-config"))]);
        let servers = gather(&config, dir.path());
        assert_eq!(servers.get("shared").unwrap().command, "from-config");
        assert_eq!(servers.get("shared").unwrap().origin, Origin::Config);
        assert_eq!(servers.get("project-only").unwrap().command, "only-here");
        // Config scope is listed first.
        assert_eq!(servers.defs[0].name, "shared");
    }

    #[test]
    fn a_config_entry_without_a_command_is_rejected_not_spawnable() {
        let config = config_with(&[("broken", cfg(""))]);
        let dir = tempdir().unwrap();
        let servers = gather(&config, dir.path());
        assert!(servers.defs.is_empty());
        assert_eq!(servers.rejected[0].reason, RejectReason::NoCommand);
    }

    /// The key must move when anything about the spawn moves, and hold still otherwise —
    /// that is the whole contract the cache leans on.
    #[test]
    fn cache_key_tracks_the_spawn_and_leaks_no_env_values() {
        let base = ServerDef {
            name: "s".into(),
            command: "npx".into(),
            args: vec!["-y".into(), "pkg".into()],
            env: BTreeMap::from([("TOKEN".to_string(), "secret-value".to_string())]),
            cwd: String::new(),
            enabled: true,
            origin: Origin::Config,
        };
        let key = base.cache_key();
        assert_eq!(key, base.clone().cache_key(), "key must be stable");
        assert!(
            !key.contains("secret-value"),
            "env values must not be stored"
        );
        assert!(!key.contains("TOKEN"));

        let mut changed = base.clone();
        changed.args.push("--flag".into());
        assert_ne!(key, changed.cache_key(), "args change the key");

        let mut moved = base.clone();
        moved.origin = Origin::Project(PathBuf::from("/elsewhere/.mcp.json"));
        assert_ne!(
            key,
            moved.cache_key(),
            "a same-named server elsewhere is not the same server"
        );

        let mut re_keyed = base.clone();
        re_keyed
            .env
            .insert("TOKEN".to_string(), "other-value".to_string());
        assert_ne!(key, re_keyed.cache_key(), "env values change the key");

        // Length-prefixing: two different splits of the same characters must not collide.
        let mut a = base.clone();
        a.args = vec!["ab".into(), "c".into()];
        let mut b = base.clone();
        b.args = vec!["a".into(), "bc".into()];
        assert_ne!(a.cache_key(), b.cache_key());
    }
}
