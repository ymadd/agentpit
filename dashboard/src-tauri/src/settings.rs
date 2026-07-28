//! Read/write the CLI configuration surface in the user's `config.toml`.
//!
//! The dashboard crate does not depend on the main `agentpit` crate, so the tiny
//! config-path rule is duplicated here (kept in sync with
//! `agentpit::config::default_config_path` in `src/config.rs`):
//! `$XDG_CONFIG_HOME/agentpit/config.toml`, else `~/.config/agentpit/config.toml`.
//!
//! Parsing uses `toml_edit` (not `toml`) so the Workflow Studio and full settings shell can
//! update only the tables they own while preserving comments, unknown future keys, and the
//! other surface's unsaved-independent fields.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml_edit::{value, Array, DocumentMut, Item, Table, Value};

/// Mirrors `WorkflowSection::default()` in `src/config.rs`.
const DEFAULT_MAX_DEPTH: u32 = 3;
const DEFAULT_MAX_CALLS_PER_MANAGER: u32 = 8;

/// `$XDG_CONFIG_HOME`, else `~/.config`. Duplicated from `src/config.rs::xdg_config_home`.
fn xdg_config_home() -> PathBuf {
    if let Ok(dir) = std::env::var("XDG_CONFIG_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    dirs::home_dir()
        .map(|home| home.join(".config"))
        .unwrap_or_else(|| PathBuf::from(".config"))
}

/// Seam for tests: build the config path from an injected config-home `base` instead of the
/// real XDG lookup, so tests never resolve to (or touch) the user's real config file.
fn config_path_from(base: Option<PathBuf>) -> PathBuf {
    let base = base.unwrap_or_else(xdg_config_home);
    base.join("agentpit").join("config.toml")
}

fn config_path() -> PathBuf {
    config_path_from(None)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowPayload {
    pub manager_backend: Option<String>,
    #[serde(default)]
    pub default_agents: Vec<String>,
    pub max_depth: u32,
    pub max_calls_per_manager: u32,
    pub use_mcp: bool,
    pub enable_ask_human: bool,
    /// Soft flow hint for the BASE workflow (sketched step order) — `[workflow].flow`.
    /// `#[serde(default)]` so a payload from an older frontend still deserializes.
    #[serde(default)]
    pub flow: Option<String>,
    /// The BASE workflow's sketched plan — `[[workflow.steps]]`.
    #[serde(default)]
    pub steps: Vec<WorkflowStepEntry>,
}

impl Default for WorkflowPayload {
    fn default() -> Self {
        Self {
            manager_backend: None,
            default_agents: Vec::new(),
            max_depth: DEFAULT_MAX_DEPTH,
            max_calls_per_manager: DEFAULT_MAX_CALLS_PER_MANAGER,
            use_mcp: false,
            enable_ask_human: false,
            flow: None,
            steps: Vec::new(),
        }
    }
}

/// One sketched plan step (`[[workflow.steps]]` / `[[workflow.types.<name>.steps]]`). Mirrors
/// `config::WorkflowStep`. Geometry (x/y/w) never crosses this boundary — the canvas layout stays
/// in the Studio's localStorage sketch; only the semantic fields become config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct WorkflowStepEntry {
    pub name: String,
    #[serde(default)]
    pub persona: Option<String>,
    #[serde(default)]
    pub behavior: Option<String>,
    #[serde(default)]
    pub manager_backend: Option<String>,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub backends: Vec<String>,
    #[serde(default)]
    pub fanout: Option<u32>,
    #[serde(default)]
    pub dynamic: bool,
    #[serde(default)]
    pub ask: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoleEntry {
    pub name: String,
    #[serde(default)]
    pub backends: Vec<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    /// Model to run this role on (e.g. "opus"). None/empty = the resolved backend's default.
    #[serde(default)]
    pub model: Option<String>,
}

/// One named workflow preset (`[workflow.types.<name>]`). Every override is optional; `roles`
/// selects a subset of the shared cast (empty = all worker roles). Mirrors `config::WorkflowType`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkflowTypeEntry {
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub roles: Vec<String>,
    #[serde(default)]
    pub manager_backend: Option<String>,
    #[serde(default)]
    pub max_depth: Option<u32>,
    #[serde(default)]
    pub max_calls_per_manager: Option<u32>,
    #[serde(default)]
    pub use_mcp: Option<bool>,
    #[serde(default)]
    pub enable_ask_human: Option<bool>,
    /// Soft flow hint (sketched step order) — written to `[workflow.types.<name>].flow`.
    #[serde(default)]
    pub flow: Option<String>,
    /// This type's sketched plan — `[[workflow.types.<name>.steps]]`. Empty = inherit the base.
    #[serde(default)]
    pub steps: Vec<WorkflowStepEntry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SettingsPayload {
    pub config_path: String,
    pub exists: bool,
    pub workflow: WorkflowPayload,
    pub roles: Vec<RoleEntry>,
    pub types: Vec<WorkflowTypeEntry>,
    /// Per-backend default model. A null value means the backend CLI chooses its own default.
    pub backend_models: BTreeMap<String, Option<String>>,
    pub known_backends: Vec<String>,
    pub reserved_type_names: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SettingsSave {
    pub workflow: WorkflowPayload,
    pub roles: Vec<RoleEntry>,
    #[serde(default)]
    pub types: Vec<WorkflowTypeEntry>,
    /// Only keys sent by the client are changed. An omitted/empty map preserves every existing
    /// backend model, keeping older dashboard frontends forward-compatible with this contract.
    #[serde(default)]
    pub backend_models: BTreeMap<String, Option<String>>,
}

// ── Full CLI configuration surface ──────────────────────────────────────────
//
// The Workflow Studio predates the rest of the settings screen and intentionally
// keeps its narrow `settings_get` / `settings_save` contract above.  The desktop
// shell uses the types below for every non-workflow field understood by the CLI.
// Keeping the contracts separate means an older Studio save cannot accidentally
// overwrite routing or ensemble values that it never loaded.

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CoreConfigPayload {
    pub backend: String,
    pub auto_route: bool,
}

impl Default for CoreConfigPayload {
    fn default() -> Self {
        Self {
            backend: "antigravity".into(),
            auto_route: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutesConfigPayload {
    pub rescue: String,
    pub review: String,
    pub explain: String,
    pub refactor: String,
}

impl Default for RoutesConfigPayload {
    fn default() -> Self {
        Self {
            rescue: "antigravity".into(),
            review: "claude".into(),
            explain: "antigravity".into(),
            refactor: "claude".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AutoRouteConfigPayload {
    pub long_context_threshold: u64,
    pub long_context_backend: String,
    #[serde(default)]
    pub review_keywords: Vec<String>,
    pub review_backend: String,
}

impl Default for AutoRouteConfigPayload {
    fn default() -> Self {
        Self {
            long_context_threshold: 100_000,
            long_context_backend: "antigravity".into(),
            review_keywords: vec![
                "review".into(),
                "audit".into(),
                "critique".into(),
                "security".into(),
            ],
            review_backend: "claude".into(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EnsembleEntryPayload {
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default)]
    pub aggregator: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EnsembleConfigPayload {
    pub default: EnsembleEntryPayload,
    pub review: EnsembleEntryPayload,
    pub security_review: EnsembleEntryPayload,
    pub adversarial_review: EnsembleEntryPayload,
    pub rescue: EnsembleEntryPayload,
    pub refactor: EnsembleEntryPayload,
}

impl Default for EnsembleConfigPayload {
    fn default() -> Self {
        Self {
            default: EnsembleEntryPayload {
                members: vec!["antigravity".into(), "claude".into(), "opencode".into()],
                aggregator: None,
            },
            review: EnsembleEntryPayload {
                members: vec!["antigravity".into(), "opencode".into()],
                aggregator: None,
            },
            security_review: EnsembleEntryPayload {
                members: vec!["claude".into(), "codex".into()],
                aggregator: None,
            },
            adversarial_review: EnsembleEntryPayload {
                members: vec!["codex".into(), "antigravity".into()],
                aggregator: None,
            },
            rescue: EnsembleEntryPayload::default(),
            refactor: EnsembleEntryPayload::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BackendConfigEntry {
    pub id: String,
    /// None means the backend's built-in transport default.
    #[serde(default)]
    pub transport: Option<String>,
    /// None means the provider CLI chooses its own model.
    #[serde(default)]
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ConfigPayload {
    pub config_path: String,
    pub exists: bool,
    pub defaults: CoreConfigPayload,
    pub routes: RoutesConfigPayload,
    pub auto_route: AutoRouteConfigPayload,
    pub ensemble: EnsembleConfigPayload,
    pub backends: Vec<BackendConfigEntry>,
    pub known_backends: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConfigSave {
    pub defaults: CoreConfigPayload,
    pub routes: RoutesConfigPayload,
    pub auto_route: AutoRouteConfigPayload,
    pub ensemble: EnsembleConfigPayload,
    #[serde(default)]
    pub backends: Vec<BackendConfigEntry>,
}

impl Default for ConfigSave {
    fn default() -> Self {
        Self {
            defaults: CoreConfigPayload::default(),
            routes: RoutesConfigPayload::default(),
            auto_route: AutoRouteConfigPayload::default(),
            ensemble: EnsembleConfigPayload::default(),
            backends: known_backends()
                .into_iter()
                .map(|id| BackendConfigEntry {
                    id,
                    transport: None,
                    model: None,
                })
                .collect(),
        }
    }
}

fn known_backends() -> Vec<String> {
    agentpit_events::BackendId::ALL
        .iter()
        .map(|b| b.as_str().to_string())
        .collect()
}

/// Workflow type names the CLI claims as `agentpit workflow` subcommands (`new` launches the
/// generator, `list` prints the catalog), so a `[workflow.types.*]` cannot use them. Shipped to
/// the UI in the payload so its client-side validator can never drift from this gate.
fn reserved_type_names() -> Vec<String> {
    vec![
        "new".to_string(),
        "list".to_string(),
        "describe".to_string(),
    ]
}

/// Read `[workflow]` / `[workflow.roles]` from `path`, tolerantly: a missing file, an
/// unparsable file, or a wrong-typed field all fall back to defaults for that field rather
/// than erroring — this command only ever informs a settings UI, never blocks on it.
fn settings_get_at(path: &Path) -> SettingsPayload {
    let exists = path.is_file();
    let raw = fs::read_to_string(path).unwrap_or_default();
    let doc = raw.parse::<DocumentMut>().unwrap_or_default();

    let mut workflow = WorkflowPayload::default();
    let mut roles = Vec::new();
    let mut types = Vec::new();
    let backend_models = known_backends()
        .into_iter()
        .map(|backend| {
            let model = doc
                .get("backends")
                .and_then(Item::as_table_like)
                .and_then(|backends| backends.get(&backend))
                .and_then(Item::as_table_like)
                .and_then(|entry| entry.get("model"))
                .and_then(Item::as_str)
                .map(str::to_string);
            (backend, model)
        })
        .collect();

    if let Some(wf) = doc.get("workflow").and_then(Item::as_table_like) {
        if let Some(v) = wf.get("manager_backend").and_then(Item::as_str) {
            workflow.manager_backend = Some(v.to_string());
        }
        if let Some(arr) = wf.get("default_agents").and_then(Item::as_array) {
            workflow.default_agents = arr
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect();
        }
        if let Some(n) = wf.get("max_depth").and_then(Item::as_integer) {
            workflow.max_depth = n.max(0) as u32;
        }
        if let Some(n) = wf.get("max_calls_per_manager").and_then(Item::as_integer) {
            workflow.max_calls_per_manager = n.max(0) as u32;
        }
        if let Some(b) = wf.get("use_mcp").and_then(Item::as_bool) {
            workflow.use_mcp = b;
        }
        if let Some(b) = wf.get("enable_ask_human").and_then(Item::as_bool) {
            workflow.enable_ask_human = b;
        }
        if let Some(s) = wf.get("flow").and_then(Item::as_str) {
            workflow.flow = Some(s.to_string());
        }
        workflow.steps = read_steps(Some(wf));
        if let Some(roles_table) = wf.get("roles").and_then(Item::as_table_like) {
            for (name, item) in roles_table.iter() {
                let backends = item
                    .get("backends")
                    .and_then(Item::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                let prompt = item
                    .get("prompt")
                    .and_then(Item::as_str)
                    .map(str::to_string);
                let model = item.get("model").and_then(Item::as_str).map(str::to_string);
                roles.push(RoleEntry {
                    name: name.to_string(),
                    backends,
                    prompt,
                    model,
                });
            }
        }
        if let Some(types_table) = wf.get("types").and_then(Item::as_table_like) {
            for (name, item) in types_table.iter() {
                let roles = item
                    .get("roles")
                    .and_then(Item::as_array)
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default();
                types.push(WorkflowTypeEntry {
                    name: name.to_string(),
                    title: item.get("title").and_then(Item::as_str).map(str::to_string),
                    description: item
                        .get("description")
                        .and_then(Item::as_str)
                        .map(str::to_string),
                    prompt: item
                        .get("prompt")
                        .and_then(Item::as_str)
                        .map(str::to_string),
                    roles,
                    manager_backend: item
                        .get("manager_backend")
                        .and_then(Item::as_str)
                        .map(str::to_string),
                    max_depth: item
                        .get("max_depth")
                        .and_then(Item::as_integer)
                        .map(|n| n.max(0) as u32),
                    max_calls_per_manager: item
                        .get("max_calls_per_manager")
                        .and_then(Item::as_integer)
                        .map(|n| n.max(0) as u32),
                    use_mcp: item.get("use_mcp").and_then(Item::as_bool),
                    enable_ask_human: item.get("enable_ask_human").and_then(Item::as_bool),
                    flow: item.get("flow").and_then(Item::as_str).map(str::to_string),
                    steps: read_steps(item.as_table_like()),
                });
            }
        }
    }

    SettingsPayload {
        config_path: path.display().to_string(),
        exists,
        workflow,
        roles,
        types,
        backend_models,
        known_backends: known_backends(),
        reserved_type_names: reserved_type_names(),
    }
}

fn table_string(table: Option<&dyn toml_edit::TableLike>, key: &str, fallback: &str) -> String {
    table
        .and_then(|table| table.get(key))
        .and_then(Item::as_str)
        .unwrap_or(fallback)
        .to_string()
}

fn table_strings(
    table: Option<&dyn toml_edit::TableLike>,
    key: &str,
    fallback: &[String],
) -> Vec<String> {
    table
        .and_then(|table| table.get(key))
        .and_then(Item::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_else(|| fallback.to_vec())
}

fn table_optional_string(table: Option<&dyn toml_edit::TableLike>, key: &str) -> Option<String> {
    table
        .and_then(|table| table.get(key))
        .and_then(Item::as_str)
        .map(str::to_string)
}

fn read_ensemble_entry(
    table: Option<&dyn toml_edit::TableLike>,
    members_key: &str,
    aggregator_key: &str,
    fallback: &EnsembleEntryPayload,
) -> EnsembleEntryPayload {
    EnsembleEntryPayload {
        members: table_strings(table, members_key, &fallback.members),
        aggregator: table_optional_string(table, aggregator_key),
    }
}

/// Read every non-workflow field supported by `src/config.rs::HubConfig`.
/// Wrong-typed or missing values fall back field-by-field, matching the forgiving
/// behaviour of the existing Workflow Studio read path.
fn config_get_at(path: &Path) -> ConfigPayload {
    let exists = path.is_file();
    let raw = fs::read_to_string(path).unwrap_or_default();
    let doc = raw.parse::<DocumentMut>().unwrap_or_default();

    let default_values = CoreConfigPayload::default();
    let defaults_table = doc.get("default").and_then(Item::as_table_like);
    let defaults = CoreConfigPayload {
        backend: table_string(defaults_table, "backend", &default_values.backend),
        auto_route: defaults_table
            .and_then(|table| table.get("auto_route"))
            .and_then(Item::as_bool)
            .unwrap_or(default_values.auto_route),
    };

    let route_defaults = RoutesConfigPayload::default();
    let routes_table = doc.get("routes").and_then(Item::as_table_like);
    let routes = RoutesConfigPayload {
        rescue: table_string(routes_table, "rescue", &route_defaults.rescue),
        review: table_string(routes_table, "review", &route_defaults.review),
        explain: table_string(routes_table, "explain", &route_defaults.explain),
        refactor: table_string(routes_table, "refactor", &route_defaults.refactor),
    };

    let auto_defaults = AutoRouteConfigPayload::default();
    let auto_table = doc.get("auto_route").and_then(Item::as_table_like);
    let auto_route = AutoRouteConfigPayload {
        long_context_threshold: auto_table
            .and_then(|table| table.get("long_context_threshold"))
            .and_then(Item::as_integer)
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or(auto_defaults.long_context_threshold),
        long_context_backend: table_string(
            auto_table,
            "long_context_backend",
            &auto_defaults.long_context_backend,
        ),
        review_keywords: table_strings(
            auto_table,
            "review_keywords",
            &auto_defaults.review_keywords,
        ),
        review_backend: table_string(auto_table, "review_backend", &auto_defaults.review_backend),
    };

    let ensemble_defaults = EnsembleConfigPayload::default();
    let ensemble_table = doc.get("ensemble").and_then(Item::as_table_like);
    let ensemble = EnsembleConfigPayload {
        default: read_ensemble_entry(
            ensemble_table,
            "default_members",
            "aggregator",
            &ensemble_defaults.default,
        ),
        review: read_ensemble_entry(
            ensemble_table,
            "review_members",
            "review_aggregator",
            &ensemble_defaults.review,
        ),
        security_review: read_ensemble_entry(
            ensemble_table,
            "security_review_members",
            "security_review_aggregator",
            &ensemble_defaults.security_review,
        ),
        adversarial_review: read_ensemble_entry(
            ensemble_table,
            "adversarial_review_members",
            "adversarial_review_aggregator",
            &ensemble_defaults.adversarial_review,
        ),
        rescue: read_ensemble_entry(
            ensemble_table,
            "rescue_members",
            "rescue_aggregator",
            &ensemble_defaults.rescue,
        ),
        refactor: read_ensemble_entry(
            ensemble_table,
            "refactor_members",
            "refactor_aggregator",
            &ensemble_defaults.refactor,
        ),
    };

    let backends_table = doc.get("backends").and_then(Item::as_table_like);
    let known = known_backends();
    let backends = known
        .iter()
        .map(|id| {
            let entry = backends_table
                .and_then(|backends| backends.get(id))
                .and_then(Item::as_table_like);
            BackendConfigEntry {
                id: id.clone(),
                transport: table_optional_string(entry, "transport"),
                model: table_optional_string(entry, "model"),
            }
        })
        .collect();

    ConfigPayload {
        config_path: path.display().to_string(),
        exists,
        defaults,
        routes,
        auto_route,
        ensemble,
        backends,
        known_backends: known,
    }
}

fn validate_config(payload: &ConfigSave) -> Result<(), String> {
    let known = known_backends();
    let check_backend = |backend: &str| -> Result<(), String> {
        if known.iter().any(|known| known == backend) {
            Ok(())
        } else {
            Err(format!("unknown backend: {backend}"))
        }
    };

    check_backend(&payload.defaults.backend)?;
    for backend in [
        &payload.routes.rescue,
        &payload.routes.review,
        &payload.routes.explain,
        &payload.routes.refactor,
        &payload.auto_route.long_context_backend,
        &payload.auto_route.review_backend,
    ] {
        check_backend(backend)?;
    }
    if payload.auto_route.long_context_threshold > i64::MAX as u64 {
        return Err("long_context_threshold exceeds TOML's integer range".into());
    }

    let ensembles = [
        &payload.ensemble.default,
        &payload.ensemble.review,
        &payload.ensemble.security_review,
        &payload.ensemble.adversarial_review,
        &payload.ensemble.rescue,
        &payload.ensemble.refactor,
    ];
    for ensemble in ensembles {
        let mut seen = std::collections::HashSet::new();
        for member in &ensemble.members {
            check_backend(member)?;
            if !seen.insert(member) {
                return Err(format!("duplicate ensemble member: {member}"));
            }
        }
        if let Some(aggregator) = &ensemble.aggregator {
            check_backend(aggregator)?;
        }
    }

    let mut seen_backends = std::collections::HashSet::new();
    for backend in &payload.backends {
        check_backend(&backend.id)?;
        if !seen_backends.insert(backend.id.as_str()) {
            return Err(format!("duplicate backend entry: {}", backend.id));
        }
        if let Some(transport) = backend.transport.as_deref() {
            if transport != "exec" && transport != "acp" {
                return Err(format!(
                    "invalid transport for {}: {transport} (expected exec or acp)",
                    backend.id
                ));
            }
        }
    }
    Ok(())
}

fn root_table_mut<'a>(doc: &'a mut DocumentMut, name: &str) -> &'a mut dyn toml_edit::TableLike {
    let item = doc
        .as_table_mut()
        .entry(name)
        .or_insert_with(|| Item::Table(Table::new()));
    if !item.is_table_like() {
        *item = Item::Table(Table::new());
    }
    item.as_table_like_mut().expect("just ensured table-like")
}

fn set_optional_string(table: &mut dyn toml_edit::TableLike, key: &str, raw: Option<&str>) {
    match raw.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value_) => set_preserving_decor(table, key, value(value_)),
        None => {
            table.remove(key);
        }
    }
}

fn set_ensemble_entry(
    table: &mut dyn toml_edit::TableLike,
    members_key: &str,
    aggregator_key: &str,
    entry: &EnsembleEntryPayload,
) {
    set_preserving_decor(
        table,
        members_key,
        Item::Value(Value::Array(string_array(&entry.members))),
    );
    set_optional_string(table, aggregator_key, entry.aggregator.as_deref());
}

/// Apply only the CLI fields represented by `ConfigSave`; workflow tables and unknown
/// future keys survive verbatim.
fn apply_config(doc: &mut DocumentMut, payload: &ConfigSave) {
    let defaults = root_table_mut(doc, "default");
    set_preserving_decor(defaults, "backend", value(payload.defaults.backend.clone()));
    set_preserving_decor(defaults, "auto_route", value(payload.defaults.auto_route));

    let routes = root_table_mut(doc, "routes");
    set_preserving_decor(routes, "rescue", value(payload.routes.rescue.clone()));
    set_preserving_decor(routes, "review", value(payload.routes.review.clone()));
    set_preserving_decor(routes, "explain", value(payload.routes.explain.clone()));
    set_preserving_decor(routes, "refactor", value(payload.routes.refactor.clone()));

    let auto_route = root_table_mut(doc, "auto_route");
    set_preserving_decor(
        auto_route,
        "long_context_threshold",
        value(payload.auto_route.long_context_threshold as i64),
    );
    set_preserving_decor(
        auto_route,
        "long_context_backend",
        value(payload.auto_route.long_context_backend.clone()),
    );
    set_preserving_decor(
        auto_route,
        "review_keywords",
        Item::Value(Value::Array(string_array(
            &payload.auto_route.review_keywords,
        ))),
    );
    set_preserving_decor(
        auto_route,
        "review_backend",
        value(payload.auto_route.review_backend.clone()),
    );

    let ensemble = root_table_mut(doc, "ensemble");
    set_ensemble_entry(
        ensemble,
        "default_members",
        "aggregator",
        &payload.ensemble.default,
    );
    set_ensemble_entry(
        ensemble,
        "review_members",
        "review_aggregator",
        &payload.ensemble.review,
    );
    set_ensemble_entry(
        ensemble,
        "security_review_members",
        "security_review_aggregator",
        &payload.ensemble.security_review,
    );
    set_ensemble_entry(
        ensemble,
        "adversarial_review_members",
        "adversarial_review_aggregator",
        &payload.ensemble.adversarial_review,
    );
    set_ensemble_entry(
        ensemble,
        "rescue_members",
        "rescue_aggregator",
        &payload.ensemble.rescue,
    );
    set_ensemble_entry(
        ensemble,
        "refactor_members",
        "refactor_aggregator",
        &payload.ensemble.refactor,
    );

    for backend in &payload.backends {
        let transport = backend
            .transport
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let model = backend
            .model
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if transport.is_none() && model.is_none() {
            if let Some(entry) = doc
                .get_mut("backends")
                .and_then(Item::as_table_like_mut)
                .and_then(|backends| backends.get_mut(&backend.id))
                .and_then(Item::as_table_like_mut)
            {
                entry.remove("transport");
                entry.remove("model");
            }
            continue;
        }
        let backends = root_table_mut(doc, "backends");
        if backends.get(&backend.id).is_none() {
            backends.insert(&backend.id, Item::Table(Table::new()));
        }
        let item = backends
            .get_mut(&backend.id)
            .expect("just inserted backend table");
        if !item.is_table_like() {
            *item = Item::Table(Table::new());
        }
        let entry = item
            .as_table_like_mut()
            .expect("just ensured backend table-like");
        set_optional_string(entry, "transport", backend.transport.as_deref());
        set_optional_string(entry, "model", backend.model.as_deref());
    }
}

fn config_save_at(payload: &ConfigSave, path: &Path) -> Result<(), String> {
    validate_config(payload)?;
    let mut doc = match fs::read_to_string(path) {
        Ok(raw) => raw
            .parse::<DocumentMut>()
            .map_err(|error| format!("failed to parse {}: {error}", path.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => DocumentMut::new(),
        Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
    };
    apply_config(&mut doc, payload);
    write_atomic(path, &doc.to_string())
}

/// Role names must match `^[a-z0-9][a-z0-9_-]*$` — lowercase-kebab, matching how they appear
/// as raw (unquoted-friendly) TOML table keys and in manager dispatch prompts.
fn valid_role_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

fn validate(payload: &SettingsSave) -> Result<(), String> {
    let known = known_backends();
    let check_backend = |b: &str| -> Result<(), String> {
        if known.iter().any(|k| k == b) {
            Ok(())
        } else {
            Err(format!("unknown backend: {b}"))
        }
    };

    if let Some(mb) = &payload.workflow.manager_backend {
        check_backend(mb)?;
    }
    for b in &payload.workflow.default_agents {
        check_backend(b)?;
    }
    for backend in payload.backend_models.keys() {
        check_backend(backend)?;
    }

    // A plan step names backends the same way everything else does, so it gets the same check.
    // Step ROLES are deliberately not validated against the cast: `workflow list` reports the
    // ones that are missing, and a half-built plan must stay saveable.
    let check_steps = |steps: &[WorkflowStepEntry], owner: &str| -> Result<(), String> {
        for s in steps {
            if let Some(b) = s.manager_backend.as_deref().filter(|b| !b.is_empty()) {
                check_backend(b).map_err(|e| format!("{owner} step {:?}: {e}", s.name))?;
            }
            for b in &s.backends {
                check_backend(b).map_err(|e| format!("{owner} step {:?}: {e}", s.name))?;
            }
        }
        Ok(())
    };
    check_steps(&payload.workflow.steps, "[workflow]")?;

    let mut seen = std::collections::HashSet::new();
    for role in &payload.roles {
        if !valid_role_name(&role.name) {
            return Err(format!(
                "invalid role name: {:?} (must match ^[a-z0-9][a-z0-9_-]*$)",
                role.name
            ));
        }
        if !seen.insert(role.name.as_str()) {
            return Err(format!("duplicate role name: {}", role.name));
        }
        for b in &role.backends {
            check_backend(b)?;
        }
    }

    let mut seen_types = std::collections::HashSet::new();
    for t in &payload.types {
        if !valid_role_name(&t.name) {
            return Err(format!(
                "invalid workflow type name: {:?} (must match ^[a-z0-9][a-z0-9_-]*$)",
                t.name
            ));
        }
        let reserved = reserved_type_names();
        if reserved.iter().any(|r| r == &t.name) {
            return Err(format!(
                "workflow type name '{}' is reserved (used by the `agentpit workflow` subcommands {})",
                t.name,
                reserved.join("/")
            ));
        }
        if !seen_types.insert(t.name.as_str()) {
            return Err(format!("duplicate workflow type name: {}", t.name));
        }
        if let Some(mb) = &t.manager_backend {
            check_backend(mb)?;
        }
        check_steps(&t.steps, &format!("workflow type '{}'", t.name))?;
    }

    Ok(())
}

fn string_array(items: &[String]) -> Array {
    let mut arr = Array::new();
    for item in items {
        arr.push(item.as_str());
    }
    arr
}

/// Read a `steps` key as plan entries. Accepts BOTH the `[[...steps]]` array-of-tables this
/// writer emits and the inline `steps = [{ name = "..." }]` form a hand-edited config may use —
/// reading only one shape would silently drop the other on the next save.
fn read_steps(parent: Option<&dyn toml_edit::TableLike>) -> Vec<WorkflowStepEntry> {
    let Some(item) = parent.and_then(|t| t.get("steps")) else {
        return Vec::new();
    };
    let tables: Vec<&dyn toml_edit::TableLike> = match item {
        Item::ArrayOfTables(arr) => arr.iter().map(|t| t as &dyn toml_edit::TableLike).collect(),
        Item::Value(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_inline_table().map(|t| t as &dyn toml_edit::TableLike))
            .collect(),
        _ => return Vec::new(),
    };
    tables
        .into_iter()
        .map(|t| WorkflowStepEntry {
            name: t
                .get("name")
                .and_then(Item::as_str)
                .unwrap_or_default()
                .to_string(),
            persona: table_optional_string(Some(t), "persona"),
            behavior: table_optional_string(Some(t), "behavior"),
            manager_backend: table_optional_string(Some(t), "manager_backend"),
            roles: table_strings(Some(t), "roles", &[]),
            backends: table_strings(Some(t), "backends", &[]),
            fanout: t
                .get("fanout")
                .and_then(Item::as_integer)
                .map(|n| n.max(0) as u32),
            dynamic: t.get("dynamic").and_then(Item::as_bool).unwrap_or(false),
            ask: t.get("ask").and_then(Item::as_bool).unwrap_or(false),
        })
        .collect()
}

/// Render plan entries as a `[[...steps]]` array of tables. Only non-empty optional fields are
/// written, so a bare step stays `name = "..."` instead of a wall of empty keys.
fn steps_array(steps: &[WorkflowStepEntry]) -> toml_edit::ArrayOfTables {
    let mut out = toml_edit::ArrayOfTables::new();
    for s in steps {
        let mut t = Table::new();
        t["name"] = value(s.name.clone());
        for (key, val) in [("persona", &s.persona), ("behavior", &s.behavior)] {
            if let Some(v) = val.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
                t[key] = value(v.to_string());
            }
        }
        if let Some(v) = s
            .manager_backend
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            t["manager_backend"] = value(v.to_string());
        }
        if !s.roles.is_empty() {
            t["roles"] = Item::Value(Value::Array(string_array(&s.roles)));
        }
        if !s.backends.is_empty() {
            t["backends"] = Item::Value(Value::Array(string_array(&s.backends)));
        }
        if let Some(v) = s.fanout {
            t["fanout"] = value(v as i64);
        }
        if s.dynamic {
            t["dynamic"] = value(true);
        }
        if s.ask {
            t["ask"] = value(true);
        }
        out.push(t);
    }
    out
}

/// Replace a table's `steps` key: the array of tables when there is a plan, otherwise remove the
/// key entirely so "no plan = inherit / no hint" survives a save from an empty canvas.
fn set_steps(table: &mut dyn toml_edit::TableLike, steps: &[WorkflowStepEntry]) {
    if steps.is_empty() {
        table.remove("steps");
    } else {
        table.insert("steps", Item::ArrayOfTables(steps_array(steps)));
    }
}

/// Apply `payload` onto `doc`'s `[workflow]` table in place: set the scalar keys (removing
/// `manager_backend` when `None`) and replace `[workflow.roles]` wholesale. Every other
/// table, key, and comment in `doc` is left untouched.
fn apply_workflow(doc: &mut DocumentMut, payload: &SettingsSave) {
    let workflow_item = doc
        .as_table_mut()
        .entry("workflow")
        .or_insert_with(|| Item::Table(Table::new()));
    // Reuse an existing table in ANY form (block `[workflow]` OR inline `workflow = {…}`) so a
    // user's inline table is not clobbered; only replace when it is not table-like at all (e.g.
    // a stray scalar). `as_table_like_mut()` matches both forms — `as_table_mut()` returns None
    // for an inline table and the old `.expect()` panicked on it (regression-tested below).
    if !workflow_item.is_table_like() {
        *workflow_item = Item::Table(Table::new());
    }
    let workflow = workflow_item
        .as_table_like_mut()
        .expect("just ensured table-like");

    match &payload.workflow.manager_backend {
        Some(backend) => set_preserving_decor(workflow, "manager_backend", value(backend.clone())),
        None => {
            workflow.remove("manager_backend");
        }
    }
    set_preserving_decor(
        workflow,
        "default_agents",
        Item::Value(Value::Array(string_array(&payload.workflow.default_agents))),
    );
    set_preserving_decor(
        workflow,
        "max_depth",
        value(payload.workflow.max_depth as i64),
    );
    set_preserving_decor(
        workflow,
        "max_calls_per_manager",
        value(payload.workflow.max_calls_per_manager as i64),
    );
    set_preserving_decor(workflow, "use_mcp", value(payload.workflow.use_mcp));
    set_preserving_decor(
        workflow,
        "enable_ask_human",
        value(payload.workflow.enable_ask_human),
    );
    // Blank flow = "no hint": remove the key rather than writing `flow = ""`, so an unsketched
    // base workflow keeps the documented "unset = no hint" semantics in config.toml.
    match payload
        .workflow
        .flow
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(flow) => set_preserving_decor(workflow, "flow", value(flow.to_string())),
        None => {
            workflow.remove("flow");
        }
    }
    set_steps(workflow, &payload.workflow.steps);

    // Replace [workflow.roles] wholesale, in the order given.
    let mut roles_table = Table::new();
    roles_table.set_implicit(true);
    for role in &payload.roles {
        let mut role_table = Table::new();
        role_table["backends"] = Item::Value(Value::Array(string_array(&role.backends)));
        // Only persist a NON-EMPTY persona. The JS layer normalizes None → "" in the settings
        // draft and sends "" verbatim for a promptless role, so writing every Some("") back
        // would (a) pollute a hand-edited config with `prompt = ""` on every unrelated save and
        // (b) make persona_task wrap the sub-task in an empty ROLE header instead of the
        // documented zero-cost passthrough. Treat blank as absent.
        if let Some(prompt) = &role.prompt {
            if !prompt.trim().is_empty() {
                role_table["prompt"] = value(prompt.clone());
            }
        }
        if let Some(model) = &role.model {
            if !model.trim().is_empty() {
                role_table["model"] = value(model.clone());
            }
        }
        roles_table[&role.name] = Item::Table(role_table);
    }
    set_preserving_decor(workflow, "roles", Item::Table(roles_table));

    // Replace [workflow.types] wholesale, in the order given. Only NON-EMPTY optional overrides
    // are written, so a promptless/rolesless type stays a minimal table rather than being padded
    // with empty strings (mirrors the role-persona handling above).
    let mut types_table = Table::new();
    types_table.set_implicit(true);
    for t in &payload.types {
        let mut tt = Table::new();
        if let Some(v) = &t.title {
            if !v.trim().is_empty() {
                tt["title"] = value(v.clone());
            }
        }
        if let Some(v) = &t.description {
            if !v.trim().is_empty() {
                tt["description"] = value(v.clone());
            }
        }
        if let Some(v) = &t.prompt {
            if !v.trim().is_empty() {
                tt["prompt"] = value(v.clone());
            }
        }
        if !t.roles.is_empty() {
            tt["roles"] = Item::Value(Value::Array(string_array(&t.roles)));
        }
        if let Some(v) = &t.manager_backend {
            if !v.is_empty() {
                tt["manager_backend"] = value(v.clone());
            }
        }
        if let Some(v) = t.max_depth {
            tt["max_depth"] = value(v as i64);
        }
        if let Some(v) = t.max_calls_per_manager {
            tt["max_calls_per_manager"] = value(v as i64);
        }
        if let Some(v) = t.use_mcp {
            tt["use_mcp"] = value(v);
        }
        if let Some(v) = t.enable_ask_human {
            tt["enable_ask_human"] = value(v);
        }
        if let Some(v) = &t.flow {
            if !v.trim().is_empty() {
                tt["flow"] = value(v.clone());
            }
        }
        set_steps(&mut tt, &t.steps);
        types_table[&t.name] = Item::Table(tt);
    }
    set_preserving_decor(workflow, "types", Item::Table(types_table));
}

/// Update only `[backends.<id>].model`. Existing transport overrides, unknown future keys, table
/// layout, and inline comments survive. Missing keys mean "leave untouched" (old frontend
/// compatibility); a present null/blank value removes only that backend's model override.
fn apply_backend_models(doc: &mut DocumentMut, models: &BTreeMap<String, Option<String>>) {
    for (backend, model) in models {
        let model = model.as_deref().map(str::trim).filter(|m| !m.is_empty());

        if model.is_none() {
            if let Some(entry) = doc
                .get_mut("backends")
                .and_then(Item::as_table_like_mut)
                .and_then(|backends| backends.get_mut(backend))
                .and_then(Item::as_table_like_mut)
            {
                entry.remove("model");
            }
            continue;
        }

        let backends_item = doc
            .as_table_mut()
            .entry("backends")
            .or_insert_with(|| Item::Table(Table::new()));
        if !backends_item.is_table_like() {
            *backends_item = Item::Table(Table::new());
        }
        let backends = backends_item
            .as_table_like_mut()
            .expect("just ensured table-like");
        if backends.get(backend).is_none() {
            backends.insert(backend, Item::Table(Table::new()));
        }
        let backend_item = backends
            .get_mut(backend)
            .expect("just inserted backend table");
        if !backend_item.is_table_like() {
            *backend_item = Item::Table(Table::new());
        }
        let backend_table = backend_item
            .as_table_like_mut()
            .expect("just ensured backend table-like");
        set_preserving_decor(
            backend_table,
            "model",
            value(model.expect("checked non-empty").to_string()),
        );
    }
}

/// Assign `item` to `key` while preserving the existing key's VALUE decor (the trailing
/// `# comment` and surrounding whitespace) when the key is already present and both old and new
/// items are values. A bare `table[key] = value(x)` replaces the whole entry including its decor,
/// silently dropping a user's inline comment on a key we rewrite — the module promises to leave
/// comments untouched, so we carry the old decor across. New keys are inserted with default decor;
/// table-valued items (e.g. `[workflow.roles]`) carry no inline value decor, so nothing is lost.
fn set_preserving_decor(table: &mut dyn toml_edit::TableLike, key: &str, mut item: Item) {
    if let Some(existing) = table.get(key) {
        if let (Some(old_val), Some(new_val)) = (existing.as_value(), item.as_value_mut()) {
            *new_val.decor_mut() = old_val.decor().clone();
        }
    }
    table.insert(key, item);
}

/// Process-wide counter so two `settings_save` calls firing close together inside the one
/// long-lived Tauri process never collide on the temp filename (PID alone is identical for both).
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn write_atomic(path: &Path, contents: &str) -> Result<(), String> {
    let dir = path
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    fs::create_dir_all(&dir).map_err(|err| format!("failed to create {}: {err}", dir.display()))?;
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = dir.join(format!(".config.toml.{}.{seq}.tmp", std::process::id()));
    if let Err(err) = fs::write(&tmp, contents) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("failed to write temp file: {err}"));
    }
    // Preserve the existing file's permission mode (e.g. a user's 0600): `fs::write` created the
    // temp at the umask default, and the rename below would otherwise silently relax a restrictive
    // config. Best-effort and Unix-only — a failure here must not fail the save.
    #[cfg(unix)]
    if let Ok(meta) = fs::metadata(path) {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&tmp, fs::Permissions::from_mode(meta.permissions().mode()));
    }
    if let Err(err) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(format!("failed to publish {}: {err}", path.display()));
    }
    Ok(())
}

fn settings_save_at(payload: &SettingsSave, path: &Path) -> Result<(), String> {
    validate(payload)?;

    let mut doc = match fs::read_to_string(path) {
        Ok(raw) => raw
            .parse::<DocumentMut>()
            .map_err(|err| format!("failed to parse {}: {err}", path.display()))?,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => DocumentMut::new(),
        Err(err) => return Err(format!("failed to read {}: {err}", path.display())),
    };

    apply_workflow(&mut doc, payload);
    apply_backend_models(&mut doc, &payload.backend_models);

    write_atomic(path, &doc.to_string())
}

#[tauri::command]
pub fn settings_get() -> Result<SettingsPayload, String> {
    Ok(settings_get_at(&config_path()))
}

#[tauri::command]
pub fn settings_save(payload: SettingsSave) -> Result<(), String> {
    settings_save_at(&payload, &config_path())
}

#[tauri::command]
pub fn config_get() -> Result<ConfigPayload, String> {
    Ok(config_get_at(&config_path()))
}

#[tauri::command]
pub fn config_save(payload: ConfigSave) -> Result<(), String> {
    config_save_at(&payload, &config_path())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wf(overrides: impl FnOnce(&mut WorkflowPayload)) -> WorkflowPayload {
        let mut w = WorkflowPayload::default();
        overrides(&mut w);
        w
    }

    #[test]
    fn config_path_from_joins_agentpit_config_toml() {
        let base = PathBuf::from("/tmp/xyz");
        assert_eq!(
            config_path_from(Some(base.clone())),
            base.join("agentpit").join("config.toml")
        );
    }

    #[test]
    fn full_config_missing_file_matches_cli_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.toml");
        let payload = config_get_at(&path);

        assert!(!payload.exists);
        assert_eq!(payload.defaults, CoreConfigPayload::default());
        assert_eq!(payload.routes, RoutesConfigPayload::default());
        assert_eq!(payload.auto_route, AutoRouteConfigPayload::default());
        assert_eq!(payload.ensemble, EnsembleConfigPayload::default());
        assert_eq!(payload.backends.len(), known_backends().len());
        assert!(payload
            .backends
            .iter()
            .all(|entry| entry.transport.is_none() && entry.model.is_none()));
    }

    #[test]
    fn saving_defaults_does_not_create_empty_backend_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        config_save_at(&ConfigSave::default(), &path).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("[backends"), "got: {raw}");
    }

    #[test]
    fn full_config_round_trip_preserves_workflow_comments_and_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            r#"# keep this header
[default]
backend = "claude" # keep inline
auto_route = true
future_key = "keep-me"

[workflow]
max_depth = 9 # untouched

[backends.codex]
transport = "exec"
model = "old-model" # keep model note
future_backend_key = 42
"#,
        )
        .unwrap();

        let mut save = ConfigSave::default();
        save.defaults.backend = "codex".into();
        save.defaults.auto_route = false;
        save.routes.review = "opencode".into();
        save.auto_route.long_context_threshold = 250_000;
        save.auto_route.review_keywords = vec!["review".into(), "threat-model".into()];
        save.ensemble.review = EnsembleEntryPayload {
            members: vec!["codex".into(), "claude".into()],
            aggregator: Some("opencode".into()),
        };
        let codex = save
            .backends
            .iter_mut()
            .find(|entry| entry.id == "codex")
            .unwrap();
        codex.transport = Some("acp".into());
        codex.model = Some("gpt-next".into());

        config_save_at(&save, &path).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("# keep this header"), "got: {raw}");
        assert!(
            raw.contains("backend = \"codex\" # keep inline"),
            "got: {raw}"
        );
        assert!(raw.contains("future_key = \"keep-me\""), "got: {raw}");
        assert!(raw.contains("max_depth = 9 # untouched"), "got: {raw}");
        assert!(raw.contains("future_backend_key = 42"), "got: {raw}");
        assert!(
            raw.contains("model = \"gpt-next\" # keep model note"),
            "got: {raw}"
        );

        let loaded = config_get_at(&path);
        assert_eq!(loaded.defaults.backend, "codex");
        assert!(!loaded.defaults.auto_route);
        assert_eq!(loaded.routes.review, "opencode");
        assert_eq!(loaded.auto_route.long_context_threshold, 250_000);
        assert_eq!(
            loaded.ensemble.review.members,
            vec!["codex".to_string(), "claude".to_string()]
        );
        assert_eq!(
            loaded.ensemble.review.aggregator.as_deref(),
            Some("opencode")
        );
        let codex = loaded
            .backends
            .iter()
            .find(|entry| entry.id == "codex")
            .unwrap();
        assert_eq!(codex.transport.as_deref(), Some("acp"));
        assert_eq!(codex.model.as_deref(), Some("gpt-next"));
    }

    #[test]
    fn full_config_rejects_unknown_backends_and_transports() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let mut unknown = ConfigSave::default();
        unknown.routes.review = "ghost".into();
        let error = config_save_at(&unknown, &path).unwrap_err();
        assert!(error.contains("unknown backend"), "got: {error}");
        assert!(!path.exists());

        let mut bad_transport = ConfigSave::default();
        bad_transport.backends[0].transport = Some("socket".into());
        let error = config_save_at(&bad_transport, &path).unwrap_err();
        assert!(error.contains("invalid transport"), "got: {error}");
        assert!(!path.exists());

        let mut overflow = ConfigSave::default();
        overflow.auto_route.long_context_threshold = u64::MAX;
        let error = config_save_at(&overflow, &path).unwrap_err();
        assert!(error.contains("integer range"), "got: {error}");
        assert!(!path.exists());
    }

    /// Pins the wire contract with dashboard/frontend/public/app.js: the payload keys are snake_case
    /// (`config_path`, `known_backends`, `manager_backend`, ...). The UI reads/writes these
    /// literal keys, so a serde rename here silently empties the settings panel — this test
    /// makes that drift a build failure instead.
    #[test]
    fn wire_contract_uses_snake_case_keys() {
        let payload = SettingsPayload {
            backend_models: BTreeMap::from([("codex".into(), Some("gpt-5.6-sol".into()))]),
            types: vec![],
            config_path: "/x/config.toml".into(),
            exists: true,
            workflow: WorkflowPayload::default(),
            roles: vec![],
            known_backends: known_backends(),
            reserved_type_names: reserved_type_names(),
        };
        let json = serde_json::to_value(&payload).unwrap();
        for key in [
            "config_path",
            "exists",
            "workflow",
            "roles",
            "types",
            "backend_models",
            "known_backends",
            "reserved_type_names",
        ] {
            assert!(
                json.get(key).is_some(),
                "missing top-level key {key}: {json}"
            );
        }
        let wf = json.get("workflow").unwrap();
        for key in [
            "manager_backend",
            "default_agents",
            "max_depth",
            "max_calls_per_manager",
            "use_mcp",
            "enable_ask_human",
            "flow",
        ] {
            assert!(wf.get(key).is_some(), "missing workflow key {key}: {wf}");
        }
        // And the save direction accepts the same snake_case keys.
        let save: SettingsSave = serde_json::from_str(
            r#"{"workflow":{"manager_backend":null,"default_agents":[],"max_depth":3,
                "max_calls_per_manager":8,"use_mcp":false,"enable_ask_human":false},
                "roles":[{"name":"reviewer","backends":["codex"],"prompt":"Critique."}]}"#,
        )
        .unwrap();
        assert_eq!(save.roles[0].name, "reviewer");
        assert!(save.backend_models.is_empty());
    }

    #[test]
    fn missing_file_returns_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent").join("config.toml");
        let payload = settings_get_at(&path);
        assert!(!payload.exists);
        assert_eq!(payload.workflow, WorkflowPayload::default());
        assert!(payload.roles.is_empty());
        assert!(payload.types.is_empty());
        assert!(payload.backend_models.values().all(Option::is_none));
        assert!(payload.known_backends.contains(&"claude".to_string()));
    }

    #[test]
    fn backend_models_round_trip_without_clobbering_transport_or_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = r#"[backends.codex]
transport = "exec"
model = "gpt-old" # keep this explanation
future_key = "preserve-me"

[backends.opencode]
transport = "acp"
model = "opencode/old"
"#;
        fs::write(&path, original).unwrap();

        let save = SettingsSave {
            backend_models: BTreeMap::from([
                ("claude".into(), Some("  claude-fable-5  ".into())),
                ("codex".into(), Some("gpt-5.6-sol".into())),
                ("opencode".into(), None),
            ]),
            types: vec![],
            workflow: WorkflowPayload::default(),
            roles: vec![],
        };
        settings_save_at(&save, &path).unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("transport = \"exec\""), "got: {raw}");
        assert!(raw.contains("transport = \"acp\""), "got: {raw}");
        assert!(raw.contains("future_key = \"preserve-me\""), "got: {raw}");
        assert!(
            raw.contains("model = \"gpt-5.6-sol\" # keep this explanation"),
            "model comment must survive; got: {raw}"
        );

        let doc = raw.parse::<DocumentMut>().unwrap();
        let models = |backend: &str| {
            doc.get("backends")
                .and_then(Item::as_table_like)
                .and_then(|backends| backends.get(backend))
                .and_then(Item::as_table_like)
                .and_then(|entry| entry.get("model"))
                .and_then(Item::as_str)
        };
        assert_eq!(models("claude"), Some("claude-fable-5"));
        assert_eq!(models("codex"), Some("gpt-5.6-sol"));
        assert_eq!(models("opencode"), None);

        let payload = settings_get_at(&path);
        assert_eq!(
            payload.backend_models["claude"].as_deref(),
            Some("claude-fable-5")
        );
        assert_eq!(
            payload.backend_models["codex"].as_deref(),
            Some("gpt-5.6-sol")
        );
        assert_eq!(payload.backend_models["opencode"], None);

        // An older frontend does not send backend_models. Its save must leave these values alone.
        let legacy_save: SettingsSave = serde_json::from_str(
            r#"{"workflow":{"manager_backend":null,"default_agents":[],"max_depth":4,
                "max_calls_per_manager":8,"use_mcp":false,"enable_ask_human":false},
                "roles":[]}"#,
        )
        .unwrap();
        settings_save_at(&legacy_save, &path).unwrap();
        let reread = settings_get_at(&path);
        assert_eq!(
            reread.backend_models["codex"].as_deref(),
            Some("gpt-5.6-sol")
        );
    }

    #[test]
    fn round_trip_preserves_unrelated_section_and_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let original = r#"# agentpit config
# a leading comment that must survive

[default]
backend = "claude" # inline comment
auto_route = true

[workflow]
manager_backend = "codex"
max_depth = 5
"#;
        fs::write(&path, original).unwrap();

        let save = SettingsSave {
            backend_models: BTreeMap::new(),
            types: vec![],
            workflow: wf(|w| {
                w.manager_backend = Some("claude".into());
                w.max_depth = 7;
            }),
            roles: vec![RoleEntry {
                name: "implementer".into(),
                backends: vec!["claude".into(), "codex".into()],
                prompt: Some("Write tests.".into()),
                model: Some("opus".into()),
            }],
        };
        settings_save_at(&save, &path).unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("model = \"opus\""),
            "role model must persist; got: {raw}"
        );
        assert!(raw.contains("# agentpit config"), "got: {raw}");
        assert!(
            raw.contains("# a leading comment that must survive"),
            "got: {raw}"
        );
        assert!(raw.contains("[default]"), "got: {raw}");
        assert!(
            raw.contains("backend = \"claude\" # inline comment"),
            "got: {raw}"
        );
        assert!(raw.contains("auto_route = true"), "got: {raw}");
        assert!(raw.contains("manager_backend = \"claude\""), "got: {raw}");
        assert!(raw.contains("max_depth = 7"), "got: {raw}");
        assert!(raw.contains("[workflow.roles.implementer]"), "got: {raw}");

        // Re-reading through settings_get_at reflects the save.
        let payload = settings_get_at(&path);
        assert_eq!(payload.workflow.manager_backend.as_deref(), Some("claude"));
        assert_eq!(payload.workflow.max_depth, 7);
        assert_eq!(payload.roles.len(), 1);
        assert_eq!(payload.roles[0].name, "implementer");
        assert_eq!(
            payload.roles[0].backends,
            vec!["claude".to_string(), "codex".to_string()]
        );
        assert_eq!(payload.roles[0].prompt.as_deref(), Some("Write tests."));
        assert_eq!(payload.roles[0].model.as_deref(), Some("opus"));
    }

    #[test]
    fn roles_add_edit_delete_reflected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let initial = SettingsSave {
            backend_models: BTreeMap::new(),
            types: vec![],
            workflow: WorkflowPayload::default(),
            roles: vec![
                RoleEntry {
                    name: "manager".into(),
                    backends: vec!["claude".into()],
                    prompt: None,
                    model: None,
                },
                RoleEntry {
                    name: "reviewer".into(),
                    backends: vec!["codex".into()],
                    prompt: Some("Critique only.".into()),
                    model: None,
                },
            ],
        };
        settings_save_at(&initial, &path).unwrap();

        // Add "implementer", edit "reviewer"'s backends, delete "manager".
        let updated = SettingsSave {
            backend_models: BTreeMap::new(),
            types: vec![],
            workflow: WorkflowPayload::default(),
            roles: vec![
                RoleEntry {
                    name: "reviewer".into(),
                    backends: vec!["codex".into(), "antigravity".into()],
                    prompt: Some("Critique only.".into()),
                    model: None,
                },
                RoleEntry {
                    name: "implementer".into(),
                    backends: vec!["claude".into()],
                    prompt: None,
                    model: None,
                },
            ],
        };
        settings_save_at(&updated, &path).unwrap();

        let payload = settings_get_at(&path);
        let names: Vec<&str> = payload.roles.iter().map(|r| r.name.as_str()).collect();
        assert!(
            !names.contains(&"manager"),
            "manager should be deleted: {names:?}"
        );
        assert!(names.contains(&"implementer"));
        let reviewer = payload.roles.iter().find(|r| r.name == "reviewer").unwrap();
        assert_eq!(
            reviewer.backends,
            vec!["codex".to_string(), "antigravity".to_string()]
        );
    }

    // The Studio's BASE canvas writes `[workflow].flow`. Blank must remove the key rather
    // than write `flow = ""`, so "unset = no hint" survives a save from an unsketched canvas.
    #[test]
    fn base_workflow_flow_round_trips_and_blank_removes_the_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let save = |flow: Option<&str>| SettingsSave {
            backend_models: BTreeMap::new(),
            types: vec![],
            roles: vec![],
            workflow: WorkflowPayload {
                flow: flow.map(str::to_string),
                ..WorkflowPayload::default()
            },
        };

        settings_save_at(&save(Some("Diagnose → Plan → Ship")), &path).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("flow = \"Diagnose → Plan → Ship\""),
            "got: {raw}"
        );
        assert_eq!(
            settings_get_at(&path).workflow.flow.as_deref(),
            Some("Diagnose → Plan → Ship")
        );

        settings_save_at(&save(Some("   ")), &path).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("flow ="), "blank flow must be removed: {raw}");
        assert_eq!(settings_get_at(&path).workflow.flow, None);
    }

    fn plan_step(name: &str) -> WorkflowStepEntry {
        WorkflowStepEntry {
            name: name.into(),
            ..WorkflowStepEntry::default()
        }
    }

    // The real JSON the Studio's Save path emits (captured from `buildPayload` in the browser
    // against the seeded canvas). Hand-written Rust structs can drift from what the frontend
    // actually sends — this pins the wire contract, including explicit `null`s for the optional
    // fields and the derived step order.
    #[test]
    fn studio_payload_json_deserializes_and_writes_the_plan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let save: SettingsSave = serde_json::from_str(
            r#"{
              "workflow": {
                "manager_backend": "claude", "default_agents": [], "max_depth": 3,
                "max_calls_per_manager": 8, "use_mcp": false, "enable_ask_human": false,
                "flow": "Diagnose → Plan",
                "steps": [
                  {"name":"Diagnose","persona":"Classify the task.","behavior":"features→category.",
                   "manager_backend":"antigravity","roles":["longctx"],"backends":[],
                   "fanout":1,"dynamic":false,"ask":false},
                  {"name":"Plan","persona":null,"behavior":null,"manager_backend":"claude",
                   "roles":["coder"],"backends":[],"fanout":null,"dynamic":true,"ask":true}
                ]
              },
              "roles": [],
              "types": []
            }"#,
        )
        .unwrap();

        assert_eq!(save.workflow.steps.len(), 2);
        assert_eq!(save.workflow.steps[1].persona, None, "explicit null → None");
        assert_eq!(save.workflow.steps[1].fanout, None);

        settings_save_at(&save, &path).unwrap();
        let back = settings_get_at(&path);
        assert_eq!(
            back.workflow.steps, save.workflow.steps,
            "round trip is lossless"
        );

        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("[[workflow.steps]]"), "got: {raw}");
        assert!(
            raw.contains("manager_backend = \"antigravity\""),
            "got: {raw}"
        );
        // nulls and falses must not be materialized as keys
        assert!(
            !raw.contains("fanout = 0") && !raw.contains("dynamic = false"),
            "got: {raw}"
        );
    }

    #[test]
    fn plan_steps_round_trip_as_an_array_of_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let full = WorkflowStepEntry {
            name: "Review".into(),
            persona: Some("Be strict.".into()),
            behavior: Some("Critique only.".into()),
            manager_backend: Some("claude".into()),
            roles: vec!["reviewer".into()],
            backends: vec!["codex".into()],
            fanout: Some(3),
            dynamic: true,
            ask: true,
        };
        let save = SettingsSave {
            backend_models: BTreeMap::new(),
            roles: vec![],
            types: vec![WorkflowTypeEntry {
                name: "review".into(),
                title: None,
                description: None,
                prompt: None,
                roles: vec![],
                manager_backend: None,
                max_depth: None,
                max_calls_per_manager: None,
                use_mcp: None,
                enable_ask_human: None,
                flow: None,
                steps: vec![plan_step("Audit")],
            }],
            workflow: WorkflowPayload {
                steps: vec![full.clone(), plan_step("Ship")],
                ..WorkflowPayload::default()
            },
        };
        settings_save_at(&save, &path).unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("[[workflow.steps]]"), "got: {raw}");
        assert!(
            raw.contains("[[workflow.types.review.steps]]"),
            "got: {raw}"
        );
        // a bare step must not be padded with empty keys
        assert!(!raw.contains("persona = \"\""), "got: {raw}");

        let back = settings_get_at(&path);
        assert_eq!(back.workflow.steps.len(), 2);
        assert_eq!(back.workflow.steps[0], full);
        assert_eq!(back.workflow.steps[1], plan_step("Ship"));
        assert_eq!(back.types[0].steps, vec![plan_step("Audit")]);

        // Saving an empty plan REMOVES the key, so "no plan = inherit" survives a round trip.
        let cleared = SettingsSave {
            workflow: WorkflowPayload::default(),
            ..save
        };
        settings_save_at(&cleared, &path).unwrap();
        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("[[workflow.steps]]"), "got: {raw}");
        assert!(settings_get_at(&path).workflow.steps.is_empty());
    }

    // A hand-edited config may use the inline form; reading only the array-of-tables shape would
    // silently drop the user's plan on the next save.
    #[test]
    fn inline_steps_array_is_read_and_normalized_on_save() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[workflow]\nsteps = [{ name = \"Diagnose\", roles = [\"longctx\"] }, { name = \"Ship\", fanout = 2 }]\n",
        )
        .unwrap();

        let read = settings_get_at(&path);
        assert_eq!(read.workflow.steps.len(), 2);
        assert_eq!(read.workflow.steps[0].name, "Diagnose");
        assert_eq!(read.workflow.steps[0].roles, vec!["longctx".to_string()]);
        assert_eq!(read.workflow.steps[1].fanout, Some(2));

        // Round-tripping it back normalizes to [[workflow.steps]] without losing anything.
        let save = SettingsSave {
            backend_models: BTreeMap::new(),
            roles: vec![],
            types: vec![],
            workflow: WorkflowPayload {
                steps: read.workflow.steps.clone(),
                ..WorkflowPayload::default()
            },
        };
        settings_save_at(&save, &path).unwrap();
        assert_eq!(settings_get_at(&path).workflow.steps, read.workflow.steps);
    }

    #[test]
    fn step_backends_are_validated_but_step_roles_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let with_steps = |steps: Vec<WorkflowStepEntry>| SettingsSave {
            backend_models: BTreeMap::new(),
            roles: vec![],
            types: vec![],
            workflow: WorkflowPayload {
                steps,
                ..WorkflowPayload::default()
            },
        };

        let bad_lead = WorkflowStepEntry {
            manager_backend: Some("nope".into()),
            ..plan_step("Review")
        };
        let err = settings_save_at(&with_steps(vec![bad_lead]), &path).unwrap_err();
        assert!(err.contains("unknown backend: nope"), "got: {err}");
        assert!(err.contains("Review"), "should name the step: {err}");

        let bad_worker = WorkflowStepEntry {
            backends: vec!["nope".into()],
            ..plan_step("Review")
        };
        assert!(settings_save_at(&with_steps(vec![bad_worker]), &path).is_err());

        // A role that is not in the cast is fine — `workflow list` reports it instead, so a
        // half-built plan stays saveable.
        let unknown_role = WorkflowStepEntry {
            roles: vec!["not-in-cast".into()],
            ..plan_step("Review")
        };
        settings_save_at(&with_steps(vec![unknown_role]), &path).unwrap();
    }

    #[test]
    fn types_round_trip_and_only_nonempty_overrides_written() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let save = SettingsSave {
            backend_models: BTreeMap::new(),
            types: vec![
                WorkflowTypeEntry {
                    name: "review".into(),
                    title: Some("Strict review".into()),
                    description: Some("Use for strict, security-focused reviews.".into()),
                    prompt: Some("Run a strict review.".into()),
                    roles: vec!["reviewer".into(), "security".into()],
                    manager_backend: Some("claude".into()),
                    max_depth: Some(2),
                    max_calls_per_manager: None,
                    use_mcp: None,
                    enable_ask_human: Some(true),
                    flow: Some("audit → refute".into()),
                    steps: vec![],
                },
                // A minimal type: just a brief, no roles/knobs — must not emit empty keys.
                WorkflowTypeEntry {
                    name: "research".into(),
                    title: None,
                    description: None,
                    prompt: Some("Research only.".into()),
                    roles: vec![],
                    manager_backend: None,
                    max_depth: None,
                    max_calls_per_manager: None,
                    use_mcp: None,
                    enable_ask_human: None,
                    flow: None,
                    steps: vec![],
                },
            ],
            workflow: WorkflowPayload::default(),
            roles: vec![],
        };
        settings_save_at(&save, &path).unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("[workflow.types.review]"), "got: {raw}");
        assert!(
            raw.contains("roles = [\"reviewer\", \"security\"]"),
            "got: {raw}"
        );
        assert!(raw.contains("enable_ask_human = true"), "got: {raw}");
        assert!(
            raw.contains("description = \"Use for strict, security-focused reviews.\""),
            "got: {raw}"
        );
        assert!(raw.contains("[workflow.types.research]"), "got: {raw}");
        // The minimal type must not be padded with empty overrides.
        let research_block = raw
            .split("[workflow.types.research]")
            .nth(1)
            .unwrap_or_default();
        assert!(
            !research_block.contains("roles ="),
            "no empty roles: {research_block}"
        );
        assert!(
            !research_block.contains("manager_backend"),
            "no empty backend: {research_block}"
        );
        assert!(
            !research_block.contains("description"),
            "no empty description: {research_block}"
        );

        let payload = settings_get_at(&path);
        assert_eq!(payload.types.len(), 2);
        let review = payload.types.iter().find(|t| t.name == "review").unwrap();
        assert_eq!(
            review.roles,
            vec!["reviewer".to_string(), "security".to_string()]
        );
        assert_eq!(review.manager_backend.as_deref(), Some("claude"));
        assert_eq!(review.max_depth, Some(2));
        assert_eq!(review.enable_ask_human, Some(true));
        assert_eq!(
            review.description.as_deref(),
            Some("Use for strict, security-focused reviews.")
        );
    }

    #[test]
    fn validation_rejects_reserved_and_duplicate_type_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let reserved = SettingsSave {
            backend_models: BTreeMap::new(),
            types: vec![WorkflowTypeEntry {
                name: "new".into(),
                title: None,
                description: None,
                prompt: None,
                roles: vec![],
                manager_backend: None,
                max_depth: None,
                max_calls_per_manager: None,
                use_mcp: None,
                enable_ask_human: None,
                flow: None,
                steps: vec![],
            }],
            workflow: WorkflowPayload::default(),
            roles: vec![],
        };
        let err = settings_save_at(&reserved, &path).unwrap_err();
        assert!(err.contains("reserved"), "got: {err}");
        assert!(!path.exists(), "must not write on validation failure");

        // `list` and `describe` are reserved too (both are `agentpit workflow` subcommands).
        for name in ["list", "describe"] {
            let mut other = reserved.clone();
            other.types[0].name = name.into();
            let err = settings_save_at(&other, &path).unwrap_err();
            assert!(err.contains("reserved"), "got: {err}");
            assert!(!path.exists(), "must not write on validation failure");
        }
    }

    #[test]
    fn validation_rejects_bad_role_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let payload = SettingsSave {
            backend_models: BTreeMap::new(),
            types: vec![],
            workflow: WorkflowPayload::default(),
            roles: vec![RoleEntry {
                name: "Bad Name!".into(),
                backends: vec![],
                prompt: None,
                model: None,
            }],
        };
        let err = settings_save_at(&payload, &path).unwrap_err();
        assert!(err.contains("invalid role name"), "got: {err}");
        assert!(!path.exists(), "must not write on validation failure");
    }

    #[test]
    fn validation_rejects_unknown_backend() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let payload = SettingsSave {
            backend_models: BTreeMap::new(),
            types: vec![],
            workflow: WorkflowPayload::default(),
            roles: vec![RoleEntry {
                name: "implementer".into(),
                backends: vec!["not-a-real-backend".into()],
                prompt: None,
                model: None,
            }],
        };
        let err = settings_save_at(&payload, &path).unwrap_err();
        assert!(err.contains("unknown backend"), "got: {err}");
    }

    #[test]
    fn validation_rejects_duplicate_role_names() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let payload = SettingsSave {
            backend_models: BTreeMap::new(),
            types: vec![],
            workflow: WorkflowPayload::default(),
            roles: vec![
                RoleEntry {
                    name: "implementer".into(),
                    backends: vec![],
                    prompt: None,
                    model: None,
                },
                RoleEntry {
                    name: "implementer".into(),
                    backends: vec![],
                    prompt: None,
                    model: None,
                },
            ],
        };
        let err = settings_save_at(&payload, &path).unwrap_err();
        assert!(err.contains("duplicate role name"), "got: {err}");
    }

    #[test]
    fn missing_file_save_creates_parent_dir_and_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("dir").join("config.toml");
        assert!(!path.parent().unwrap().exists());

        let payload = SettingsSave {
            backend_models: BTreeMap::new(),
            types: vec![],
            workflow: WorkflowPayload::default(),
            roles: vec![],
        };
        settings_save_at(&payload, &path).unwrap();

        assert!(path.exists());
        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("[workflow]"), "got: {raw}");
    }

    /// Regression for the HIGH finding: an inline `workflow = { … }` table (valid TOML the CLI
    /// loader accepts identically to a block table) must not panic on save. Before the fix
    /// `as_table_mut().expect(...)` panicked because `is_table_like()` accepts an inline table
    /// but `as_table_mut()` returns None for it.
    #[test]
    fn inline_workflow_table_saves_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "workflow = { manager_backend = \"codex\", max_depth = 5 }\n",
        )
        .unwrap();

        let save = SettingsSave {
            backend_models: BTreeMap::new(),
            types: vec![],
            workflow: wf(|w| {
                w.manager_backend = Some("claude".into());
                w.max_depth = 9;
            }),
            roles: vec![],
        };
        settings_save_at(&save, &path).unwrap();

        let payload = settings_get_at(&path);
        assert_eq!(payload.workflow.manager_backend.as_deref(), Some("claude"));
        assert_eq!(payload.workflow.max_depth, 9);
    }

    /// Regression: rewriting a scalar `[workflow]` key must keep the user's inline `# comment`
    /// on that key. Before the fix, `workflow[key] = value(x)` replaced the whole entry decor.
    #[test]
    fn save_preserves_inline_comment_on_a_rewritten_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(
            &path,
            "[workflow]\nmanager_backend = \"codex\" # pinned for cost\nmax_depth = 3\n",
        )
        .unwrap();

        let save = SettingsSave {
            backend_models: BTreeMap::new(),
            types: vec![],
            workflow: wf(|w| w.manager_backend = Some("claude".into())),
            roles: vec![],
        };
        settings_save_at(&save, &path).unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("manager_backend = \"claude\" # pinned for cost"),
            "inline comment must survive a key rewrite; got: {raw}"
        );
    }

    /// Regression for the promptless-role finding: the JS layer sends `prompt: Some("")` for a
    /// role with no persona; that must NOT be written as `prompt = ""` (which would both pollute
    /// the file and defeat persona_task's passthrough). settings_get_at reads it back as None.
    #[test]
    fn empty_prompt_is_not_written_and_reads_back_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let save = SettingsSave {
            backend_models: BTreeMap::new(),
            types: vec![],
            workflow: WorkflowPayload::default(),
            roles: vec![RoleEntry {
                name: "implementer".into(),
                backends: vec!["claude".into()],
                prompt: Some(String::new()), // what the UI sends for a promptless role
                model: None,
            }],
        };
        settings_save_at(&save, &path).unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        assert!(
            !raw.contains("prompt ="),
            "must not write an empty prompt; got: {raw}"
        );
        let payload = settings_get_at(&path);
        assert_eq!(payload.roles.len(), 1);
        assert_eq!(payload.roles[0].prompt, None);
    }

    /// Regression for the permission finding: a restrictive mode on an existing config (0600)
    /// must survive a save rather than being relaxed to the umask default.
    #[cfg(unix)]
    #[test]
    fn save_preserves_existing_file_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "[workflow]\nmax_depth = 3\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();

        let save = SettingsSave {
            backend_models: BTreeMap::new(),
            types: vec![],
            workflow: wf(|w| w.max_depth = 4),
            roles: vec![],
        };
        settings_save_at(&save, &path).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "restrictive mode must be preserved, got {mode:o}"
        );
    }

    /// The duplicated defaults here must match `WorkflowSection::default()` in `src/config.rs`.
    /// The dashboard crate deliberately does not depend on the main crate, so this pins the
    /// literals against drift at least on this side (documented mirror; update both together).
    #[test]
    fn duplicated_defaults_match_documented_values() {
        assert_eq!(DEFAULT_MAX_DEPTH, 3);
        assert_eq!(DEFAULT_MAX_CALLS_PER_MANAGER, 8);
        let d = WorkflowPayload::default();
        assert_eq!(d.max_depth, 3);
        assert_eq!(d.max_calls_per_manager, 8);
        assert!(!d.use_mcp && !d.enable_ask_human);
    }
}
