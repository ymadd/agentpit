//! Read/write `[workflow]` + `[workflow.roles]` in the user's `config.toml`.
//!
//! The dashboard crate does not depend on the main `agentpit` crate, so the tiny
//! config-path rule is duplicated here (kept in sync with
//! `agentpit::config::default_config_path` in `src/config.rs`):
//! `$XDG_CONFIG_HOME/agentpit/config.toml`, else `~/.config/agentpit/config.toml`.
//!
//! Parsing uses `toml_edit` (not `toml`) so `settings_save` can rewrite only the
//! `[workflow]` scalars and replace `[workflow.roles]` wholesale while leaving every
//! other table/comment in the file byte-for-byte untouched.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml_edit::{Array, DocumentMut, Item, Table, Value, value};

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
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoleEntry {
    pub name: String,
    #[serde(default)]
    pub backends: Vec<String>,
    #[serde(default)]
    pub prompt: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SettingsPayload {
    pub config_path: String,
    pub exists: bool,
    pub workflow: WorkflowPayload,
    pub roles: Vec<RoleEntry>,
    pub known_backends: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SettingsSave {
    pub workflow: WorkflowPayload,
    pub roles: Vec<RoleEntry>,
}

fn known_backends() -> Vec<String> {
    agentpit_events::BackendId::ALL
        .iter()
        .map(|b| b.as_str().to_string())
        .collect()
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
                let prompt = item.get("prompt").and_then(Item::as_str).map(str::to_string);
                roles.push(RoleEntry {
                    name: name.to_string(),
                    backends,
                    prompt,
                });
            }
        }
    }

    SettingsPayload {
        config_path: path.display().to_string(),
        exists,
        workflow,
        roles,
        known_backends: known_backends(),
    }
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

    Ok(())
}

fn string_array(items: &[String]) -> Array {
    let mut arr = Array::new();
    for item in items {
        arr.push(item.as_str());
    }
    arr
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
    set_preserving_decor(workflow, "max_depth", value(payload.workflow.max_depth as i64));
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
        roles_table[&role.name] = Item::Table(role_table);
    }
    set_preserving_decor(workflow, "roles", Item::Table(roles_table));
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
    fs::create_dir_all(&dir)
        .map_err(|err| format!("failed to create {}: {err}", dir.display()))?;
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

    /// Pins the wire contract with dashboard/ui/app.js: the payload keys are snake_case
    /// (`config_path`, `known_backends`, `manager_backend`, ...). The UI reads/writes these
    /// literal keys, so a serde rename here silently empties the settings panel — this test
    /// makes that drift a build failure instead.
    #[test]
    fn wire_contract_uses_snake_case_keys() {
        let payload = SettingsPayload {
            config_path: "/x/config.toml".into(),
            exists: true,
            workflow: WorkflowPayload::default(),
            roles: vec![],
            known_backends: known_backends(),
        };
        let json = serde_json::to_value(&payload).unwrap();
        for key in ["config_path", "exists", "workflow", "roles", "known_backends"] {
            assert!(json.get(key).is_some(), "missing top-level key {key}: {json}");
        }
        let wf = json.get("workflow").unwrap();
        for key in [
            "manager_backend",
            "default_agents",
            "max_depth",
            "max_calls_per_manager",
            "use_mcp",
            "enable_ask_human",
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
    }

    #[test]
    fn missing_file_returns_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent").join("config.toml");
        let payload = settings_get_at(&path);
        assert!(!payload.exists);
        assert_eq!(payload.workflow, WorkflowPayload::default());
        assert!(payload.roles.is_empty());
        assert!(payload.known_backends.contains(&"claude".to_string()));
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
            workflow: wf(|w| {
                w.manager_backend = Some("claude".into());
                w.max_depth = 7;
            }),
            roles: vec![RoleEntry {
                name: "implementer".into(),
                backends: vec!["claude".into(), "codex".into()],
                prompt: Some("Write tests.".into()),
            }],
        };
        settings_save_at(&save, &path).unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        assert!(raw.contains("# agentpit config"), "got: {raw}");
        assert!(raw.contains("# a leading comment that must survive"), "got: {raw}");
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
    }

    #[test]
    fn roles_add_edit_delete_reflected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");

        let initial = SettingsSave {
            workflow: WorkflowPayload::default(),
            roles: vec![
                RoleEntry {
                    name: "manager".into(),
                    backends: vec!["claude".into()],
                    prompt: None,
                },
                RoleEntry {
                    name: "reviewer".into(),
                    backends: vec!["codex".into()],
                    prompt: Some("Critique only.".into()),
                },
            ],
        };
        settings_save_at(&initial, &path).unwrap();

        // Add "implementer", edit "reviewer"'s backends, delete "manager".
        let updated = SettingsSave {
            workflow: WorkflowPayload::default(),
            roles: vec![
                RoleEntry {
                    name: "reviewer".into(),
                    backends: vec!["codex".into(), "antigravity".into()],
                    prompt: Some("Critique only.".into()),
                },
                RoleEntry {
                    name: "implementer".into(),
                    backends: vec!["claude".into()],
                    prompt: None,
                },
            ],
        };
        settings_save_at(&updated, &path).unwrap();

        let payload = settings_get_at(&path);
        let names: Vec<&str> = payload.roles.iter().map(|r| r.name.as_str()).collect();
        assert!(!names.contains(&"manager"), "manager should be deleted: {names:?}");
        assert!(names.contains(&"implementer"));
        let reviewer = payload.roles.iter().find(|r| r.name == "reviewer").unwrap();
        assert_eq!(
            reviewer.backends,
            vec!["codex".to_string(), "antigravity".to_string()]
        );
    }

    #[test]
    fn validation_rejects_bad_role_name() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let payload = SettingsSave {
            workflow: WorkflowPayload::default(),
            roles: vec![RoleEntry {
                name: "Bad Name!".into(),
                backends: vec![],
                prompt: None,
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
            workflow: WorkflowPayload::default(),
            roles: vec![RoleEntry {
                name: "implementer".into(),
                backends: vec!["not-a-real-backend".into()],
                prompt: None,
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
            workflow: WorkflowPayload::default(),
            roles: vec![
                RoleEntry {
                    name: "implementer".into(),
                    backends: vec![],
                    prompt: None,
                },
                RoleEntry {
                    name: "implementer".into(),
                    backends: vec![],
                    prompt: None,
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
        fs::write(&path, "workflow = { manager_backend = \"codex\", max_depth = 5 }\n").unwrap();

        let save = SettingsSave {
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
            workflow: WorkflowPayload::default(),
            roles: vec![RoleEntry {
                name: "implementer".into(),
                backends: vec!["claude".into()],
                prompt: Some(String::new()), // what the UI sends for a promptless role
            }],
        };
        settings_save_at(&save, &path).unwrap();

        let raw = fs::read_to_string(&path).unwrap();
        assert!(!raw.contains("prompt ="), "must not write an empty prompt; got: {raw}");
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
            workflow: wf(|w| w.max_depth = 4),
            roles: vec![],
        };
        settings_save_at(&save, &path).unwrap();

        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "restrictive mode must be preserved, got {mode:o}");
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
