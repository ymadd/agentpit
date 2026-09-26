//! Blueprint files (design §9.1, §17 P3): `<project>/.agentpit/blueprints/<name>.json` and
//! `~/.config/agentpit/blueprints/<name>.json`.
//!
//! Saving is optimistic: the client says which revision it edited (`base_rev`), and a save
//! over a file that has moved on since is refused with the current revision, so two
//! editors never silently overwrite each other. The document is written back as given —
//! keys this build does not know survive the round trip — in a stable, readable key order.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    blueprint_rev, is_valid_blueprint_name, validate, BlueprintScope, Diagnostic, Severity,
    ValidateEnv, MAX_DOC_BYTES,
};

/// `$XDG_CONFIG_HOME/agentpit/blueprints` or `~/.config/agentpit/blueprints`.
pub fn user_blueprints_dir() -> PathBuf {
    let base = match std::env::var("XDG_CONFIG_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => dirs::home_dir()
            .map(|h| h.join(".config"))
            .unwrap_or_else(|| PathBuf::from(".config")),
    };
    base.join("agentpit").join("blueprints")
}

/// `<project>/.agentpit/blueprints`.
pub fn project_blueprints_dir(project: &Path) -> PathBuf {
    project.join(".agentpit").join("blueprints")
}

/// Where blueprints are looked up, most specific first: a project's shadow the user's on a
/// name collision (design Q2).
pub fn blueprint_dirs(project: Option<&Path>) -> Vec<(BlueprintScope, PathBuf)> {
    let mut dirs = Vec::new();
    if let Some(p) = project {
        dirs.push((BlueprintScope::Project, project_blueprints_dir(p)));
    }
    dirs.push((BlueprintScope::User, user_blueprints_dir()));
    dirs
}

/// One blueprint file, as a list shows it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BlueprintEntry {
    /// The file name without `.json`.
    pub name: String,
    pub scope: BlueprintScope,
    pub path: String,
    pub rev: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Validates without errors (it can be started).
    pub runnable: bool,
    #[serde(default)]
    pub errors: usize,
    #[serde(default)]
    pub warnings: usize,
    /// A project blueprint of the same name hides this one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shadowed: bool,
    pub modified_ms: u64,
}

/// A blueprint file with its document and validation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredBlueprint {
    pub entry: BlueprintEntry,
    pub doc: Value,
    pub diagnostics: Vec<Diagnostic>,
}

#[derive(Debug)]
pub enum StoreError {
    NotFound,
    /// The file moved on since `base_rev` (or exists, for a create). `current` is its
    /// revision now, `None` when it was deleted.
    Conflict {
        current: Option<String>,
    },
    Invalid(String),
    Io(std::io::Error),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::NotFound => write!(f, "no such blueprint"),
            StoreError::Conflict { current: Some(rev) } => write!(
                f,
                "the blueprint changed since you opened it (now {rev}); reload it and redo your edit"
            ),
            StoreError::Conflict { current: None } => write!(
                f,
                "the blueprint was deleted since you opened it; save it under a name again"
            ),
            StoreError::Invalid(why) => write!(f, "{why}"),
            StoreError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
    fn from(e: std::io::Error) -> Self {
        if e.kind() == std::io::ErrorKind::NotFound {
            StoreError::NotFound
        } else {
            StoreError::Io(e)
        }
    }
}

fn modified_ms(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_millis() as u64)
}

/// Read a blueprint file: a regular file, at most [`MAX_DOC_BYTES`], JSON.
pub fn read_doc(path: &Path) -> Result<Value, StoreError> {
    use std::io::Read;
    let meta = std::fs::metadata(path)?;
    if !meta.is_file() {
        return Err(StoreError::Invalid(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(MAX_DOC_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_DOC_BYTES {
        return Err(StoreError::Invalid(format!(
            "{} is larger than {MAX_DOC_BYTES} bytes",
            path.display()
        )));
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| StoreError::Invalid(format!("{} is not JSON: {e}", path.display())))
}

fn entry_for(
    name: &str,
    scope: BlueprintScope,
    path: &Path,
    doc: &Value,
    env: &ValidateEnv,
) -> (BlueprintEntry, Vec<Diagnostic>) {
    let v = validate(doc, env);
    let count = |sev| v.diagnostics.iter().filter(|d| d.severity == sev).count();
    let text = |key: &str| doc.get(key).and_then(Value::as_str).map(str::to_string);
    (
        BlueprintEntry {
            name: name.to_string(),
            scope,
            path: path.display().to_string(),
            rev: v.rev.clone(),
            title: text("title"),
            description: text("description"),
            runnable: v.is_runnable(),
            errors: count(Severity::Error),
            warnings: count(Severity::Warning),
            shadowed: false,
            modified_ms: modified_ms(path),
        },
        v.diagnostics,
    )
}

/// Every blueprint in `dirs` (see [`blueprint_dirs`]), sorted by name, the shadowed ones
/// after the one that hides them. Files that are not `<valid-name>.json` or do not parse
/// are skipped.
pub fn list_blueprints(
    dirs: &[(BlueprintScope, PathBuf)],
    env: &ValidateEnv,
) -> Vec<BlueprintEntry> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for (scope, dir) in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut here: Vec<BlueprintEntry> = entries
            .filter_map(Result::ok)
            .filter_map(|e| {
                let path = e.path();
                let name = path
                    .file_name()?
                    .to_str()?
                    .strip_suffix(".json")?
                    .to_string();
                if !is_valid_blueprint_name(&name) {
                    return None;
                }
                let doc = read_doc(&path).ok()?;
                Some(entry_for(&name, *scope, &path, &doc, env).0)
            })
            .collect();
        for e in &mut here {
            e.shadowed = !seen.insert(e.name.clone());
        }
        out.extend(here);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name).then(a.shadowed.cmp(&b.shadowed)));
    out
}

/// The file a blueprint of `name` lives in, in `dir`.
pub fn blueprint_path(dir: &Path, name: &str) -> Result<PathBuf, StoreError> {
    if !is_valid_blueprint_name(name) {
        return Err(StoreError::Invalid(format!(
            "{name:?} is not a blueprint name; use [a-z0-9_-], starting with a letter or digit"
        )));
    }
    Ok(dir.join(format!("{name}.json")))
}

/// Read one blueprint with its validation.
pub fn get_blueprint(
    scope: BlueprintScope,
    dir: &Path,
    name: &str,
    env: &ValidateEnv,
) -> Result<StoredBlueprint, StoreError> {
    let path = blueprint_path(dir, name)?;
    let doc = read_doc(&path)?;
    let (entry, diagnostics) = entry_for(name, scope, &path, &doc, env);
    Ok(StoredBlueprint {
        entry,
        doc,
        diagnostics,
    })
}

/// Save `doc` as `<dir>/<name>.json`.
///
/// - `base_rev: None` creates: refused with `Conflict` when the file exists.
/// - `base_rev: Some(rev)` updates: refused with `Conflict` unless the file's current
///   revision is `rev`. A layout-only change does not change the revision (it is excluded
///   from it), so two people dragging nodes never conflict.
///
/// Saving does not require the document to validate (drafts are saved too); the result
/// carries its diagnostics. The document's `name` must be the file name.
pub fn save_blueprint(
    scope: BlueprintScope,
    dir: &Path,
    name: &str,
    doc: &Value,
    base_rev: Option<&str>,
    env: &ValidateEnv,
) -> Result<StoredBlueprint, StoreError> {
    let path = blueprint_path(dir, name)?;
    if doc.get("name").and_then(Value::as_str) != Some(name) {
        return Err(StoreError::Invalid(format!(
            "the document's \"name\" must be {name:?} to be saved as {name}.json"
        )));
    }
    let text = pretty(doc);
    if text.len() > MAX_DOC_BYTES {
        return Err(StoreError::Invalid(format!(
            "the blueprint is larger than {MAX_DOC_BYTES} bytes; split it"
        )));
    }
    let current = match read_doc(&path) {
        Ok(existing) => Some(blueprint_rev(&existing)),
        Err(StoreError::NotFound) => None,
        // An unreadable file can still be replaced by an update that names it.
        Err(StoreError::Invalid(_)) => Some(String::new()),
        Err(e) => return Err(e),
    };
    match (base_rev, &current) {
        (None, None) => {}
        (Some(base), Some(now)) if base == now => {}
        _ => {
            return Err(StoreError::Conflict {
                current: current.filter(|c| !c.is_empty()),
            });
        }
    }
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{name}.json.tmp"));
    std::fs::write(&tmp, text.as_bytes())?;
    std::fs::rename(&tmp, &path)?;
    get_blueprint(scope, dir, name, env)
}

/// Delete `<dir>/<name>.json` if it is still at `base_rev`.
pub fn delete_blueprint(dir: &Path, name: &str, base_rev: &str) -> Result<(), StoreError> {
    let path = blueprint_path(dir, name)?;
    let current = blueprint_rev(&read_doc(&path)?);
    if current != base_rev {
        return Err(StoreError::Conflict {
            current: Some(current),
        });
    }
    std::fs::remove_file(&path)?;
    Ok(())
}

// ---------------------------------------------------------------------------------------
// Readable output.

/// Top-level keys in the order people read a blueprint; anything else follows, sorted.
const DOC_KEYS: &[&str] = &[
    "schema",
    "name",
    "title",
    "description",
    "requires",
    "inputs",
    "workspace",
    "budget",
    "policy",
    "nodes",
    "edges",
    "layout",
];
/// Node and edge keys first in this order.
const ITEM_KEYS: &[&str] = &[
    "id",
    "title",
    "kind",
    "parent",
    "from",
    "to",
    "on",
    "label",
    "role",
    "backend",
    "model",
    "effort",
    "category",
    "access",
    "verdict",
    "task",
    "prompt",
    "command",
    "cwd",
    "options",
    "timeout_secs",
    "on_timeout",
    "retries",
    "max_iterations",
    "feedback",
];

/// Two-space JSON with blueprint keys in reading order (the revision ignores key order,
/// so this is purely cosmetic). Every key is kept.
pub fn pretty(doc: &Value) -> String {
    let mut out = String::new();
    write_pretty(doc, 0, Some(DOC_KEYS), &mut out);
    out.push('\n');
    out
}

fn ordered_keys<'a>(map: &'a serde_json::Map<String, Value>, order: &[&str]) -> Vec<&'a String> {
    let mut keys: Vec<&String> = map.keys().collect();
    keys.sort_by_key(|k| {
        (
            order.iter().position(|o| o == k).unwrap_or(order.len()),
            (*k).clone(),
        )
    });
    keys
}

fn write_pretty(v: &Value, depth: usize, order: Option<&[&str]>, out: &mut String) {
    let pad = |n: usize| "  ".repeat(n);
    match v {
        Value::Object(map) if map.is_empty() => out.push_str("{}"),
        Value::Object(map) => {
            out.push_str("{\n");
            let keys = ordered_keys(map, order.unwrap_or(&[]));
            for (i, k) in keys.iter().enumerate() {
                out.push_str(&pad(depth + 1));
                out.push_str(&serde_json::to_string(k).unwrap_or_default());
                out.push_str(": ");
                // nodes / edges hold items with their own reading order.
                let child_order = match (depth, k.as_str()) {
                    (0, "nodes" | "edges") => Some(ITEM_KEYS),
                    _ => None,
                };
                write_pretty(&map[k.as_str()], depth + 1, child_order, out);
                if i + 1 < keys.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&pad(depth));
            out.push('}');
        }
        Value::Array(items) if items.is_empty() => out.push_str("[]"),
        Value::Array(items) => {
            out.push_str("[\n");
            for (i, item) in items.iter().enumerate() {
                out.push_str(&pad(depth + 1));
                write_pretty(item, depth + 1, order, out);
                if i + 1 < items.len() {
                    out.push(',');
                }
                out.push('\n');
            }
            out.push_str(&pad(depth));
            out.push(']');
        }
        scalar => out.push_str(&serde_json::to_string(scalar).unwrap_or_default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc() -> Value {
        json!({
            "name": "fix",
            "schema": "agentpit.blueprint/1",
            "nodes": [{"kind": "check", "id": "t", "command": "true", "x_editor_note": "keep me"}],
            "future_key": {"b": 1, "a": [1.5, 2]},
            "layout": {"t": {"x": 10.25, "y": 20}}
        })
    }

    #[test]
    fn saves_are_optimistic_and_keep_unknown_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("bp");
        let env = ValidateEnv::default();
        let created =
            save_blueprint(BlueprintScope::User, &dir, "fix", &doc(), None, &env).unwrap();
        assert!(created.entry.runnable, "{:?}", created.diagnostics);
        // Unknown keys (top level and inside a node) and floats survive the round trip.
        let back = get_blueprint(BlueprintScope::User, &dir, "fix", &env).unwrap();
        assert_eq!(back.doc, doc());
        let text = std::fs::read_to_string(dir.join("fix.json")).unwrap();
        assert!(
            text.starts_with("{\n  \"schema\": \"agentpit.blueprint/1\",\n  \"name\": \"fix\""),
            "{text}"
        );
        assert!(text.find("\"id\"").unwrap() < text.find("\"kind\"").unwrap());

        // Creating over an existing file conflicts.
        let err =
            save_blueprint(BlueprintScope::User, &dir, "fix", &doc(), None, &env).unwrap_err();
        assert!(
            matches!(err, StoreError::Conflict { current: Some(ref r) } if *r == created.entry.rev)
        );

        // Update from the right base, then a stale base conflicts with the new revision.
        let mut edited = doc();
        edited["title"] = json!("Fix it");
        let saved = save_blueprint(
            BlueprintScope::User,
            &dir,
            "fix",
            &edited,
            Some(&created.entry.rev),
            &env,
        )
        .unwrap();
        assert_ne!(saved.entry.rev, created.entry.rev);
        let err = save_blueprint(
            BlueprintScope::User,
            &dir,
            "fix",
            &doc(),
            Some(&created.entry.rev),
            &env,
        )
        .unwrap_err();
        assert!(
            matches!(err, StoreError::Conflict { current: Some(ref r) } if *r == saved.entry.rev)
        );

        // A layout-only change keeps the revision, so it never conflicts with itself.
        let mut moved = edited.clone();
        moved["layout"]["t"]["x"] = json!(99);
        let again = save_blueprint(
            BlueprintScope::User,
            &dir,
            "fix",
            &moved,
            Some(&saved.entry.rev),
            &env,
        )
        .unwrap();
        assert_eq!(again.entry.rev, saved.entry.rev);

        // Delete needs the current revision too.
        assert!(matches!(
            delete_blueprint(&dir, "fix", "b1-stale"),
            Err(StoreError::Conflict { .. })
        ));
        delete_blueprint(&dir, "fix", &again.entry.rev).unwrap();
        assert!(matches!(
            get_blueprint(BlueprintScope::User, &dir, "fix", &env),
            Err(StoreError::NotFound)
        ));
    }

    #[test]
    fn names_and_sizes_are_checked_before_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let env = ValidateEnv::default();
        let err = save_blueprint(BlueprintScope::User, tmp.path(), "../x", &doc(), None, &env)
            .unwrap_err();
        assert!(matches!(err, StoreError::Invalid(_)));
        let err = save_blueprint(
            BlueprintScope::User,
            tmp.path(),
            "other",
            &doc(),
            None,
            &env,
        )
        .unwrap_err();
        assert!(err.to_string().contains("\"name\""), "{err}");
        let mut big = doc();
        big["future_key"] = json!("x".repeat(MAX_DOC_BYTES));
        assert!(matches!(
            save_blueprint(BlueprintScope::User, tmp.path(), "fix", &big, None, &env),
            Err(StoreError::Invalid(_))
        ));
        assert!(
            std::fs::read_dir(tmp.path()).unwrap().next().is_none(),
            "nothing written"
        );
    }

    #[test]
    fn a_project_blueprint_shadows_the_users() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("p");
        let user = tmp.path().join("u");
        let env = ValidateEnv::default();
        let dirs = vec![
            (BlueprintScope::Project, project_blueprints_dir(&project)),
            (BlueprintScope::User, user.clone()),
        ];
        save_blueprint(
            BlueprintScope::Project,
            &dirs[0].1,
            "fix",
            &doc(),
            None,
            &env,
        )
        .unwrap();
        save_blueprint(BlueprintScope::User, &user, "fix", &doc(), None, &env).unwrap();
        let mut other = doc();
        other["name"] = json!("aaa");
        save_blueprint(BlueprintScope::User, &user, "aaa", &other, None, &env).unwrap();
        std::fs::write(user.join("Not A Name.json"), "{}").unwrap();
        std::fs::write(user.join("broken.json"), "{").unwrap();
        let list = list_blueprints(&dirs, &env);
        let rows: Vec<(&str, BlueprintScope, bool)> = list
            .iter()
            .map(|e| (e.name.as_str(), e.scope, e.shadowed))
            .collect();
        assert_eq!(
            rows,
            [
                ("aaa", BlueprintScope::User, false),
                ("fix", BlueprintScope::Project, false),
                ("fix", BlueprintScope::User, true),
            ]
        );
    }
}
