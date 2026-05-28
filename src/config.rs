use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::types::{BackendId, Transport};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, clap::ValueEnum,
)]
#[serde(rename_all = "lowercase")]
#[clap(rename_all = "lowercase")]
pub enum RouteKey {
    Rescue,
    Review,
    Explain,
    Refactor,
}

impl RouteKey {
    pub fn as_str(&self) -> &'static str {
        match self {
            RouteKey::Rescue => "rescue",
            RouteKey::Review => "review",
            RouteKey::Explain => "explain",
            RouteKey::Refactor => "refactor",
        }
    }
}

impl std::fmt::Display for RouteKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DefaultSection {
    #[serde(default = "default_backend")]
    pub backend: BackendId,
    #[serde(default = "default_true")]
    pub auto_route: bool,
}

impl Default for DefaultSection {
    fn default() -> Self {
        Self {
            backend: default_backend(),
            auto_route: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AutoRouteSection {
    #[serde(default = "default_long_context_threshold")]
    pub long_context_threshold: u64,
    #[serde(default = "default_backend")]
    pub long_context_backend: BackendId,
    #[serde(default = "default_review_keywords")]
    pub review_keywords: Vec<String>,
    #[serde(default = "default_review_backend")]
    pub review_backend: BackendId,
}

impl Default for AutoRouteSection {
    fn default() -> Self {
        Self {
            long_context_threshold: default_long_context_threshold(),
            long_context_backend: default_backend(),
            review_keywords: default_review_keywords(),
            review_backend: default_review_backend(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnsembleSection {
    #[serde(default = "default_ensemble_members")]
    pub default_members: Vec<BackendId>,
    #[serde(default)]
    pub aggregator: Option<BackendId>,
    #[serde(default = "default_review_members")]
    pub review_members: Vec<BackendId>,
    #[serde(default)]
    pub review_aggregator: Option<BackendId>,
    #[serde(default = "default_security_review_members")]
    pub security_review_members: Vec<BackendId>,
    #[serde(default)]
    pub security_review_aggregator: Option<BackendId>,
    #[serde(default = "default_adversarial_review_members")]
    pub adversarial_review_members: Vec<BackendId>,
    #[serde(default)]
    pub adversarial_review_aggregator: Option<BackendId>,
    #[serde(default)]
    pub rescue_members: Vec<BackendId>,
    #[serde(default)]
    pub rescue_aggregator: Option<BackendId>,
    #[serde(default)]
    pub refactor_members: Vec<BackendId>,
    #[serde(default)]
    pub refactor_aggregator: Option<BackendId>,
}

impl Default for EnsembleSection {
    fn default() -> Self {
        Self {
            default_members: default_ensemble_members(),
            aggregator: None,
            review_members: default_review_members(),
            review_aggregator: None,
            security_review_members: default_security_review_members(),
            security_review_aggregator: None,
            adversarial_review_members: default_adversarial_review_members(),
            adversarial_review_aggregator: None,
            rescue_members: Vec::new(),
            rescue_aggregator: None,
            refactor_members: Vec::new(),
            refactor_aggregator: None,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackendOverride {
    #[serde(default)]
    pub transport: Option<Transport>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HubConfig {
    #[serde(default)]
    pub default: DefaultSection,
    #[serde(default = "default_routes")]
    pub routes: BTreeMap<RouteKey, BackendId>,
    #[serde(default)]
    pub auto_route: AutoRouteSection,
    #[serde(default)]
    pub ensemble: EnsembleSection,
    #[serde(default)]
    pub backends: BTreeMap<BackendId, BackendOverride>,
}

fn default_backend() -> BackendId {
    BackendId::Antigravity
}
fn default_review_backend() -> BackendId {
    BackendId::Claude
}
fn default_true() -> bool {
    true
}
fn default_long_context_threshold() -> u64 {
    100_000
}
fn default_review_keywords() -> Vec<String> {
    vec![
        "review".into(),
        "audit".into(),
        "critique".into(),
        "security".into(),
    ]
}
fn default_ensemble_members() -> Vec<BackendId> {
    vec![
        BackendId::Antigravity,
        BackendId::Claude,
        BackendId::Opencode,
    ]
}
fn default_review_members() -> Vec<BackendId> {
    vec![BackendId::Antigravity, BackendId::Opencode]
}
fn default_security_review_members() -> Vec<BackendId> {
    vec![BackendId::Claude, BackendId::Codex]
}
fn default_adversarial_review_members() -> Vec<BackendId> {
    // Codex for adversarial scrutiny; Antigravity's long context for whole-file tracing.
    vec![BackendId::Codex, BackendId::Antigravity]
}
fn default_routes() -> BTreeMap<RouteKey, BackendId> {
    let mut m = BTreeMap::new();
    m.insert(RouteKey::Rescue, BackendId::Antigravity);
    m.insert(RouteKey::Review, BackendId::Claude);
    m.insert(RouteKey::Explain, BackendId::Antigravity);
    m.insert(RouteKey::Refactor, BackendId::Claude);
    m
}

pub fn save_config(config: &HubConfig) -> Result<PathBuf> {
    let path = default_config_path();
    save_config_at(config, &path)?;
    Ok(path)
}

pub fn save_config_at(config: &HubConfig, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let raw = toml::to_string_pretty(config)
        .with_context(|| format!("failed to serialize config for {}", path.display()))?;
    fs::write(path, raw).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    File,
    Defaults,
}

impl ConfigSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConfigSource::File => "file",
            ConfigSource::Defaults => "defaults",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: HubConfig,
    pub source: ConfigSource,
    pub path: PathBuf,
}

pub fn xdg_config_home() -> PathBuf {
    if let Ok(dir) = env::var("XDG_CONFIG_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    dirs::home_dir()
        .map(|h| h.join(".config"))
        .unwrap_or_else(|| PathBuf::from(".config"))
}

pub fn default_config_path() -> PathBuf {
    xdg_config_home().join("agentpit").join("config.toml")
}

pub fn expand_env(value: &mut toml::Value) {
    match value {
        toml::Value::String(s) => {
            *s = expand_env_string(s);
        }
        toml::Value::Array(items) => {
            for v in items {
                expand_env(v);
            }
        }
        toml::Value::Table(table) => {
            for (_, v) in table.iter_mut() {
                expand_env(v);
            }
        }
        _ => {}
    }
}

fn expand_env_string(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after_open = &rest[start + 2..];
        if let Some(end) = after_open.find('}') {
            let name = &after_open[..end];
            let valid =
                !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
            if valid {
                if let Ok(val) = env::var(name) {
                    out.push_str(&val);
                }
                rest = &after_open[end + 1..];
                continue;
            }
        }
        out.push_str("${");
        rest = after_open;
    }
    out.push_str(rest);
    out
}

pub fn load_config(override_path: Option<&Path>) -> Result<LoadedConfig> {
    let path = override_path
        .map(PathBuf::from)
        .unwrap_or_else(default_config_path);

    match fs::read_to_string(&path) {
        Ok(raw) => {
            let mut value: toml::Value = toml::from_str(&raw)
                .with_context(|| format!("Failed to load {}", path.display()))?;
            expand_env(&mut value);
            let config: HubConfig = value
                .try_into()
                .with_context(|| format!("Failed to load {}", path.display()))?;
            Ok(LoadedConfig {
                config,
                source: ConfigSource::File,
                path,
            })
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(LoadedConfig {
            config: HubConfig {
                routes: default_routes(),
                ..HubConfig::default()
            },
            source: ConfigSource::Defaults,
            path,
        }),
        Err(err) => Err(anyhow!("Failed to read {}: {err}", path.display())),
    }
}

pub const DEFAULT_CONFIG_TOML: &str = r#"# agentpit config
# Backends currently available: antigravity (agy), gemini, claude, codex (paid plan), opencode

[default]
backend = "antigravity"
auto_route = true

[routes]
rescue   = "antigravity"
review   = "claude"
explain  = "antigravity"
refactor = "claude"

[auto_route]
long_context_threshold = 100000
long_context_backend   = "antigravity"
review_keywords        = ["review", "audit", "critique", "security"]
review_backend         = "claude"

[ensemble]
# Generic ensemble members + optional aggregator
default_members = ["antigravity", "claude", "opencode"]
# aggregator = "claude"

# Per-tool overrides
review_members = ["antigravity", "opencode"]
# review_aggregator = "claude"

# Per-backend transport override.
# [backends.antigravity]
# transport = "acp"
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn returns_defaults_when_file_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("absent.toml");
        let loaded = load_config(Some(&path)).unwrap();
        assert_eq!(loaded.source, ConfigSource::Defaults);
        assert_eq!(loaded.config.default.backend, BackendId::Antigravity);
        assert_eq!(
            loaded.config.ensemble.review_members,
            vec![BackendId::Antigravity, BackendId::Opencode]
        );
    }

    #[test]
    fn parses_toml_overrides() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"
[default]
backend = "claude"
auto_route = false

[ensemble]
review_members = ["claude", "gemini"]
"#,
        )
        .unwrap();
        let loaded = load_config(Some(&path)).unwrap();
        assert_eq!(loaded.source, ConfigSource::File);
        assert_eq!(loaded.config.default.backend, BackendId::Claude);
        assert!(!loaded.config.default.auto_route);
        assert_eq!(
            loaded.config.ensemble.review_members,
            vec![BackendId::Claude, BackendId::Gemini]
        );
    }

    #[test]
    fn expands_env_references() {
        // SAFETY: tests run single-threaded by default via test isolation;
        // this is a controlled local env var.
        unsafe {
            env::set_var("AGENTPIT_TEST_BACKEND", "opencode");
        }
        let dir = tempdir().unwrap();
        let path = dir.path().join("env.toml");
        fs::write(
            &path,
            r#"
[default]
backend = "${AGENTPIT_TEST_BACKEND}"
"#,
        )
        .unwrap();
        let loaded = load_config(Some(&path)).unwrap();
        assert_eq!(loaded.config.default.backend, BackendId::Opencode);
        unsafe {
            env::remove_var("AGENTPIT_TEST_BACKEND");
        }
    }

    #[test]
    fn expand_env_handles_multiple_variables_and_unknowns() {
        unsafe {
            env::set_var("AGENTPIT_TEST_A", "alpha");
            env::set_var("AGENTPIT_TEST_B", "beta");
            env::remove_var("AGENTPIT_TEST_UNKNOWN");
        }
        let mut value: toml::Value =
            toml::from_str(r#"items = ["${AGENTPIT_TEST_A}-${AGENTPIT_TEST_B}", "${AGENTPIT_TEST_UNKNOWN}-fallback"]"#)
                .unwrap();
        expand_env(&mut value);
        let items = value.get("items").unwrap().as_array().unwrap();
        assert_eq!(items[0].as_str().unwrap(), "alpha-beta");
        assert_eq!(items[1].as_str().unwrap(), "-fallback");
        unsafe {
            env::remove_var("AGENTPIT_TEST_A");
            env::remove_var("AGENTPIT_TEST_B");
        }
    }

    #[test]
    fn expand_env_ignores_malformed_placeholders() {
        let mut value: toml::Value =
            toml::from_str(r#"v = "no ${closing brace here""#).unwrap();
        expand_env(&mut value);
        assert_eq!(
            value.get("v").unwrap().as_str().unwrap(),
            "no ${closing brace here"
        );
    }

    #[test]
    fn expand_env_preserves_non_ascii_characters() {
        unsafe {
            env::set_var("AGENTPIT_TEST_GREETING", "こんにちは");
        }
        let mut value: toml::Value =
            toml::from_str(r#"v = "日本語 ${AGENTPIT_TEST_GREETING} — café""#).unwrap();
        expand_env(&mut value);
        assert_eq!(
            value.get("v").unwrap().as_str().unwrap(),
            "日本語 こんにちは — café"
        );
        unsafe {
            env::remove_var("AGENTPIT_TEST_GREETING");
        }
    }

    #[test]
    fn rejects_invalid_backend() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        fs::write(
            &path,
            r#"
[routes]
review = "imaginary-backend"
"#,
        )
        .unwrap();
        let err = load_config(Some(&path)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Failed to load"), "got: {msg}");
    }
}
