use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::types::{BackendId, Transport};

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    clap::ValueEnum,
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
    /// Run every `agentpit rescue` as a cost-ladder cascade (same as `rescue --cascade`).
    #[serde(default)]
    pub cascade: bool,
}

impl Default for DefaultSection {
    fn default() -> Self {
        Self {
            backend: default_backend(),
            auto_route: true,
            cascade: false,
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
    /// Profile-route cost tiebreak: candidates scoring within this margin of the best are
    /// interchangeable on quality, and the cheapest (`[backends.<id>].cost`) wins.
    #[serde(default = "default_quality_margin")]
    pub quality_margin: u8,
    /// kNN similarity routing (`[auto_route.similarity]`): route to the backend that won
    /// similar past tasks. Only active in `--features similarity` builds with the embedding
    /// model downloaded (`agentpit similarity init`).
    #[serde(default)]
    pub similarity: SimilaritySection,
}

/// Knobs for the kNN similarity routing layer. All thresholds deliberately conservative:
/// when the evidence is thin the layer falls through to the profile route.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimilaritySection {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Neighbours consulted per lookup.
    #[serde(default = "default_similarity_k")]
    pub k: usize,
    /// Cosine similarity below this is not "the same kind of task".
    #[serde(default = "default_similarity_min_sim")]
    pub min_sim: f32,
    /// The winning backend needs at least this many similar samples.
    #[serde(default = "default_similarity_min_samples")]
    pub min_samples: usize,
    /// The winner's win-rate lead over the runner-up must be at least this.
    #[serde(default = "default_similarity_margin")]
    pub margin: f32,
    /// Give up on (lazy) model load + query embedding after this long and fall through.
    #[serde(default = "default_similarity_load_timeout_ms")]
    pub load_timeout_ms: u64,
}

impl Default for SimilaritySection {
    fn default() -> Self {
        Self {
            enabled: true,
            k: default_similarity_k(),
            min_sim: default_similarity_min_sim(),
            min_samples: default_similarity_min_samples(),
            margin: default_similarity_margin(),
            load_timeout_ms: default_similarity_load_timeout_ms(),
        }
    }
}

fn default_similarity_k() -> usize {
    8
}
fn default_similarity_min_sim() -> f32 {
    0.80
}
fn default_similarity_min_samples() -> usize {
    3
}
fn default_similarity_margin() -> f32 {
    0.15
}
fn default_similarity_load_timeout_ms() -> u64 {
    300
}

impl Default for AutoRouteSection {
    fn default() -> Self {
        Self {
            long_context_threshold: default_long_context_threshold(),
            long_context_backend: default_backend(),
            review_keywords: default_review_keywords(),
            review_backend: default_review_backend(),
            quality_margin: default_quality_margin(),
            similarity: SimilaritySection::default(),
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

/// One named workflow role: a persona the manager dispatches to, bound to a backend
/// preference list. The reserved name `manager` (see `workflow::roles::MANAGER_ROLE`)
/// configures the orchestrator itself; every other role is a worker.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoleConfig {
    /// Backend preference order; the first available one wins. Empty = any available backend.
    #[serde(default)]
    pub backends: Vec<BackendId>,
    /// Persona preamble prepended to every task dispatched to this role (appended to the
    /// orchestrator prompt for the `manager` role).
    #[serde(default)]
    pub prompt: Option<String>,
    /// Model to run this role on (e.g. "opus", "gpt-5-codex"). `None` = the resolved backend's
    /// own default. Overridden by an explicit `--model` at dispatch, and falls back to the
    /// backend's `[backends.<id>].model` when unset. Passed to the backend CLI's model flag.
    #[serde(default)]
    pub model: Option<String>,
}

/// A named workflow "type" (`[workflow.types.<name>]`) — a PRESET over the base `[workflow]`
/// and the shared `[workflow.roles.*]` cast, selected by `agentpit workflow <type> "<goal>"`.
/// Every field is an optional override: unset fields inherit from `[workflow]`, and an empty
/// `roles` list means "every configured worker role" (same roster as the base workflow). The
/// cast itself is never duplicated — a type only *chooses* which shared roles it dispatches to.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkflowType {
    /// Human-readable label for dashboards/UX (the config key is the machine name).
    #[serde(default)]
    pub title: Option<String>,
    /// When-to-use description: a short human/agent-facing summary of what this workflow is for and
    /// when it is effective, to aid selecting the right type. Surfaced by `workflow list`; unlike
    /// `prompt` (the manager's runtime instruction), this is documentation, not a directive.
    #[serde(default)]
    pub description: Option<String>,
    /// The workflow BRIEF: high-level instructions for the manager in this type, injected as a
    /// dedicated block above the roster. Composes with any `[workflow.roles.manager]` persona.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Which worker roles (from the shared cast) this type dispatches to, in order. Empty = every
    /// configured worker role. Names not present in `[workflow.roles.*]` are skipped with a warning.
    #[serde(default)]
    pub roles: Vec<String>,
    /// Override the manager backend for this type. Unset = base `[workflow].manager_backend`.
    #[serde(default)]
    pub manager_backend: Option<BackendId>,
    #[serde(default)]
    pub max_depth: Option<u32>,
    #[serde(default)]
    pub max_calls_per_manager: Option<u32>,
    #[serde(default)]
    pub use_mcp: Option<bool>,
    #[serde(default)]
    pub enable_ask_human: Option<bool>,
    /// A soft "suggested flow" for this type — the ordered step names the user sketched on the
    /// dashboard canvas (distilled from the drawn edges). Injected into the manager prompt as a
    /// non-binding hint ("you sketched this; adapt freely"); the manager still improvises. Unset =
    /// no flow hint (the default). Never a hard DAG — this stays model-driven.
    #[serde(default)]
    pub flow: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowSection {
    #[serde(default)]
    pub manager_backend: Option<BackendId>,
    #[serde(default)]
    pub default_agents: Vec<BackendId>,
    /// Named roles (`[workflow.roles.<name>]`). Empty = legacy flat-backend roster.
    #[serde(default)]
    pub roles: BTreeMap<String, RoleConfig>,
    /// Named workflow presets (`[workflow.types.<name>]`), selected by `agentpit workflow <type>`.
    /// Empty = only the base `[workflow]` (invoked as `agentpit workflow "<goal>"`).
    #[serde(default)]
    pub types: BTreeMap<String, WorkflowType>,
    #[serde(default = "default_workflow_max_depth")]
    pub max_depth: u32,
    #[serde(default = "default_workflow_max_calls")]
    pub max_calls_per_manager: u32,
    /// Orchestrate via the MCP channel (`agentpit mcp serve`) instead of shelling out.
    /// Overridden by the `--use-mcp` CLI flag. Only the claude manager supports MCP mode.
    #[serde(default)]
    pub use_mcp: bool,
    /// Inject the human back-channel into the manager (the `ask_human` MCP tool / `agentpit ask`
    /// CLI) and the question-discipline prompt that governs it. Default OFF until dogfooded: with
    /// it off the manager is never told to call a back-channel that would otherwise 404.
    #[serde(default)]
    pub enable_ask_human: bool,
}

impl Default for WorkflowSection {
    fn default() -> Self {
        Self {
            manager_backend: None,
            default_agents: Vec::new(),
            roles: BTreeMap::new(),
            types: BTreeMap::new(),
            max_depth: 3,
            max_calls_per_manager: 8,
            use_mcp: false,
            enable_ask_human: false,
        }
    }
}

fn default_workflow_max_depth() -> u32 {
    3
}
fn default_workflow_max_calls() -> u32 {
    8
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackendOverride {
    #[serde(default)]
    pub transport: Option<Transport>,
    /// Default model for this backend (e.g. "opus", "gpt-5-codex"). `None` = the CLI's own
    /// default. This is the lowest-precedence source: `--model` and a role's `model` both win.
    #[serde(default)]
    pub model: Option<String>,
    /// Relative cost on a 0–100 scale (0 = free, e.g. a local model). `None` = unranked:
    /// the router's cost tiebreak treats it as mid-range (50) so an unconfigured backend
    /// neither wins nor loses on cost alone.
    #[serde(default)]
    pub cost: Option<u8>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HubConfig {
    #[serde(default)]
    pub default: DefaultSection,
    /// Hard pins per tool. An entry here wins over `auto_route` **entirely** — the
    /// similarity / capability-profile / cost-tiebreak stages never run for that tool — so
    /// this defaults to EMPTY (opt-in). It used to default to a pin for every `RouteKey`,
    /// which made the whole auto-route chain unreachable no matter what the user configured.
    #[serde(default)]
    pub routes: BTreeMap<RouteKey, BackendId>,
    #[serde(default)]
    pub auto_route: AutoRouteSection,
    #[serde(default)]
    pub ensemble: EnsembleSection,
    #[serde(default)]
    pub workflow: WorkflowSection,
    #[serde(default)]
    pub backends: BTreeMap<BackendId, BackendOverride>,
    #[serde(default)]
    pub cascade: CascadeSection,
}

/// Used for both `default.backend` and `auto_route.long_context_backend`. Claude since
/// 2026-07-26: antigravity's individual tier hits week-long quota blocks, which makes it a
/// poor default for exactly the dispatches that have nowhere else to go.
fn default_backend() -> BackendId {
    BackendId::Claude
}
fn default_review_backend() -> BackendId {
    BackendId::Claude
}
fn default_quality_margin() -> u8 {
    5
}

/// `[cascade]`: escalate a failed cheap dispatch up the quality ladder (`rescue --cascade`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CascadeSection {
    /// Shell command run in the task's cwd after a hop succeeds; a non-zero exit fails the
    /// hop anyway (e.g. "cargo test"). Unset = trust the dispatch outcome.
    #[serde(default)]
    pub verify: Option<String>,
    /// Escalations after the first hop (2 = up to three backends total).
    #[serde(default = "default_cascade_max_hops")]
    pub max_hops: u32,
    /// Only backends scoring at least this on the diagnosed category join the ladder.
    #[serde(default = "default_cascade_min_score")]
    pub min_score: u8,
}

impl Default for CascadeSection {
    fn default() -> Self {
        Self {
            verify: None,
            max_hops: default_cascade_max_hops(),
            min_score: default_cascade_min_score(),
        }
    }
}

fn default_cascade_max_hops() -> u32 {
    2
}
fn default_cascade_min_score() -> u8 {
    60
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
    if let Ok(dir) = env::var("XDG_CONFIG_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
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
            config: HubConfig::default(),
            source: ConfigSource::Defaults,
            path,
        }),
        Err(err) => Err(anyhow!("Failed to read {}: {err}", path.display())),
    }
}

pub const DEFAULT_CONFIG_TOML: &str = r#"# agentpit config
# Backends currently available: antigravity (agy), claude, codex (paid plan), opencode

[default]
backend = "claude"
auto_route = true

# [routes] — OPTIONAL hard pins, one per tool. An entry here wins over auto_route
# entirely: the capability-profile / similarity / cost-tiebreak stages never run for that
# tool, so `agentpit profile learn` can never influence it. Leave a tool out (or omit this
# section) to let agentpit pick the backend from measured capability instead.
# [routes]
# rescue   = "antigravity"
# review   = "claude"

[auto_route]
long_context_threshold = 100000
long_context_backend   = "claude"
review_keywords        = ["review", "audit", "critique", "security"]
review_backend         = "claude"

[ensemble]
# Generic ensemble members + optional aggregator
default_members = ["antigravity", "claude", "opencode"]
# aggregator = "claude"

# Per-tool overrides
review_members = ["antigravity", "opencode"]
# review_aggregator = "claude"

# [workflow]
# Model-driven workflow: a manager backend (claude|codex) orchestrates sub-agents.
# manager_backend       = "claude"   # unset: first authenticated manager; supported [default], then claude/codex
# default_agents        = ["antigravity", "opencode", "codex"]  # defaults to all available minus the manager
# max_depth             = 3          # hard recursion ceiling, enforced in Rust; clamped to 1..=32
# max_calls_per_manager = 8          # advisory sub-dispatch budget surfaced in the prompt
# use_mcp               = false      # orchestrate via the MCP channel (claude manager only); --use-mcp overrides

# Named workflow roles: the manager dispatches to ROLES instead of raw backends when any
# worker role is defined. The reserved role "manager" configures the orchestrator itself.
# [workflow.roles.manager]
# backends = ["claude"]              # first supported manager (claude|codex) in the list wins
# prompt   = "Prefer small, verifiable steps."
#
# [workflow.roles.implementer]
# backends = ["claude", "codex"]     # preference order; first AVAILABLE backend wins
# prompt   = "You are the implementer. Write the smallest correct change with tests."
# model    = "opus"                  # optional: pin this role's model (--model wins; else backend default)
#
# [workflow.roles.reviewer]
# backends = ["codex", "antigravity"]
# prompt   = "You are a strict reviewer. Critique only; do not rewrite."

# Named workflow presets: `agentpit workflow <type> "<goal>"` selects one. A type is a PRESET
# over [workflow] and the shared cast above — it picks which roles to use, gives the manager a
# brief, and may override knobs. Omitting the type runs the base [workflow]. The names `new`,
# `list`, and `describe` are reserved (they are `agentpit workflow` subcommands). Roles are never
# duplicated here.
# [workflow.types.review]
# title    = "Strict code review"
# description = "Use when a change needs a strict, security-focused review."  # when-to-use (shown by `workflow list`)
# prompt   = "Run a strict review: find spec violations, boundary bugs, and security issues."
# roles    = ["reviewer", "security"]   # subset of the shared cast; empty/omitted = all worker roles
# manager_backend = "claude"            # optional per-type override
# enable_ask_human = true               # optional per-type knob override

# Per-backend transport + default model override.
# [backends.antigravity]
# transport = "acp"
# model     = "gemini-3-pro"   # default model for this backend (lowest precedence; --model / role.model win)
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
        assert_eq!(loaded.config.default.backend, BackendId::Claude);
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
review_members = ["claude", "opencode"]
"#,
        )
        .unwrap();
        let loaded = load_config(Some(&path)).unwrap();
        assert_eq!(loaded.source, ConfigSource::File);
        assert_eq!(loaded.config.default.backend, BackendId::Claude);
        assert!(!loaded.config.default.auto_route);
        assert_eq!(
            loaded.config.ensemble.review_members,
            vec![BackendId::Claude, BackendId::Opencode]
        );
    }

    /// Regression: `routes` used to default to a pin for EVERY `RouteKey`, and the default
    /// applied whenever the `[routes]` section was absent. Since the router evaluates the
    /// route table before `auto_route`, that made the similarity / capability-profile /
    /// cost-tiebreak stages unreachable — omitting `[routes]` re-created the pins instead of
    /// clearing them. A config without the section must now leave the table empty.
    #[test]
    fn routes_default_to_empty_so_auto_route_is_reachable() {
        let dir = tempdir().unwrap();

        let path = dir.path().join("no-routes.toml");
        fs::write(&path, "[default]\nbackend = \"claude\"\n").unwrap();
        let loaded = load_config(Some(&path)).unwrap();
        assert!(
            loaded.config.routes.is_empty(),
            "omitting [routes] must not synthesize pins: {:?}",
            loaded.config.routes
        );

        // No config file at all: same rule.
        let missing = dir.path().join("absent.toml");
        let defaults = load_config(Some(&missing)).unwrap();
        assert_eq!(defaults.source, ConfigSource::Defaults);
        assert!(defaults.config.routes.is_empty());

        // An explicit pin still works, and only for the tool it names.
        let pinned_path = dir.path().join("pinned.toml");
        fs::write(&pinned_path, "[routes]\nreview = \"codex\"\n").unwrap();
        let pinned = load_config(Some(&pinned_path)).unwrap();
        assert_eq!(
            pinned.config.routes.get(&RouteKey::Review),
            Some(&BackendId::Codex)
        );
        assert_eq!(pinned.config.routes.get(&RouteKey::Rescue), None);
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
        let mut value: toml::Value = toml::from_str(r#"v = "no ${closing brace here""#).unwrap();
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
    fn workflow_section_defaults_when_absent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("empty.toml");
        fs::write(&path, "").unwrap();
        let loaded = load_config(Some(&path)).unwrap();
        assert_eq!(loaded.config.workflow.max_depth, 3);
        assert_eq!(loaded.config.workflow.max_calls_per_manager, 8);
        assert!(loaded.config.workflow.manager_backend.is_none());
        assert!(loaded.config.workflow.default_agents.is_empty());
    }

    #[test]
    fn workflow_section_parses_overrides() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("workflow.toml");
        fs::write(
            &path,
            r#"
[workflow]
manager_backend = "codex"
default_agents = ["opencode", "opencode"]
max_depth = 5
max_calls_per_manager = 12
"#,
        )
        .unwrap();
        let loaded = load_config(Some(&path)).unwrap();
        assert_eq!(
            loaded.config.workflow.manager_backend,
            Some(BackendId::Codex)
        );
        assert_eq!(
            loaded.config.workflow.default_agents,
            vec![BackendId::Opencode, BackendId::Opencode]
        );
        assert_eq!(loaded.config.workflow.max_depth, 5);
        assert_eq!(loaded.config.workflow.max_calls_per_manager, 12);
    }

    #[test]
    fn workflow_roles_default_empty_and_parse_tables() {
        let dir = tempdir().unwrap();
        let empty = dir.path().join("empty.toml");
        fs::write(&empty, "").unwrap();
        assert!(
            load_config(Some(&empty))
                .unwrap()
                .config
                .workflow
                .roles
                .is_empty()
        );

        let path = dir.path().join("roles.toml");
        fs::write(
            &path,
            r#"
[workflow.roles.manager]
backends = ["claude"]
prompt = "Prefer small steps."

[workflow.roles.reviewer]
backends = ["codex", "antigravity"]

[workflow.roles.researcher]
prompt = "You research."
"#,
        )
        .unwrap();
        let roles = load_config(Some(&path)).unwrap().config.workflow.roles;
        assert_eq!(roles.len(), 3);
        assert_eq!(roles["manager"].backends, vec![BackendId::Claude]);
        assert_eq!(
            roles["manager"].prompt.as_deref(),
            Some("Prefer small steps.")
        );
        assert_eq!(
            roles["reviewer"].backends,
            vec![BackendId::Codex, BackendId::Antigravity]
        );
        assert!(roles["reviewer"].prompt.is_none());
        // A role may omit backends entirely (any available backend qualifies).
        assert!(roles["researcher"].backends.is_empty());
    }

    #[test]
    fn workflow_types_default_empty_and_parse_presets() {
        let dir = tempdir().unwrap();
        let empty = dir.path().join("empty.toml");
        fs::write(&empty, "").unwrap();
        assert!(
            load_config(Some(&empty))
                .unwrap()
                .config
                .workflow
                .types
                .is_empty()
        );

        let path = dir.path().join("types.toml");
        fs::write(
            &path,
            r#"
[workflow.roles.reviewer]
backends = ["codex"]

[workflow.types.review]
title = "Strict code review"
prompt = "Run a strict review."
roles = ["reviewer", "security"]
manager_backend = "claude"
enable_ask_human = true

[workflow.types.research]
prompt = "Research only."
"#,
        )
        .unwrap();
        let wf = load_config(Some(&path)).unwrap().config.workflow;
        assert_eq!(wf.types.len(), 2);
        let review = &wf.types["review"];
        assert_eq!(review.title.as_deref(), Some("Strict code review"));
        assert_eq!(review.prompt.as_deref(), Some("Run a strict review."));
        assert_eq!(
            review.roles,
            vec!["reviewer".to_string(), "security".to_string()]
        );
        assert_eq!(review.manager_backend, Some(BackendId::Claude));
        assert_eq!(review.enable_ask_human, Some(true));
        // Unset per-type knobs stay None (they inherit from [workflow] at resolution time).
        assert!(review.max_depth.is_none());
        // A minimal type may set just a brief; roles omitted = all worker roles.
        assert!(wf.types["research"].roles.is_empty());
        assert!(wf.types["research"].manager_backend.is_none());
    }

    #[test]
    fn sample_config_types_example_parses_when_uncommented() {
        // The commented [workflow.types.*] example must stay valid TOML when uncommented.
        let block: String = DEFAULT_CONFIG_TOML
            .lines()
            .skip_while(|l| !l.starts_with("# [workflow.types.review]"))
            .take_while(|l| l.starts_with('#'))
            .map(|l| {
                l.strip_prefix("# ")
                    .or_else(|| l.strip_prefix("#"))
                    .unwrap_or(l)
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !block.is_empty(),
            "types example block not found in sample config"
        );
        let parsed: HubConfig = toml::from_str(&block).expect("uncommented types example parses");
        let review = parsed
            .workflow
            .types
            .get("review")
            .expect("review type present");
        assert_eq!(
            review.roles,
            vec!["reviewer".to_string(), "security".to_string()]
        );
        assert_eq!(review.manager_backend, Some(BackendId::Claude));
    }

    #[test]
    fn sample_config_roles_example_parses_when_uncommented() {
        // The commented [workflow.roles.*] example in DEFAULT_CONFIG_TOML must stay valid TOML
        // when a user uncomments it. Extract the example block (from the first roles table to
        // the end of the commented run) and strip the comment markers.
        let block: String = DEFAULT_CONFIG_TOML
            .lines()
            .skip_while(|l| !l.starts_with("# [workflow.roles.manager]"))
            .take_while(|l| l.starts_with('#'))
            .map(|l| {
                l.strip_prefix("# ")
                    .or_else(|| l.strip_prefix("#"))
                    .unwrap_or(l)
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !block.is_empty(),
            "roles example block not found in sample config"
        );
        let parsed: HubConfig = toml::from_str(&block).expect("uncommented roles example parses");
        assert!(parsed.workflow.roles.contains_key("manager"));
        assert!(parsed.workflow.roles.contains_key("implementer"));
        assert!(parsed.workflow.roles.contains_key("reviewer"));
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
