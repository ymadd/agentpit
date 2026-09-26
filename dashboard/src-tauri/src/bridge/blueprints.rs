//! Blueprint files for the Studio: list, read, save with `base_rev` (optimistic
//! concurrency: a save over someone else's change is a `conflict`, never a silent
//! overwrite), delete, and validate. Keys the Studio does not know survive a round trip:
//! the document is saved as given, and the store keeps every key.
//!
//! The files are on this machine, so these go through the shared store in
//! `agentpit_events::loops` directly; no daemon is involved.

use std::path::{Path, PathBuf};

use agentpit_events::loops::{
    blueprint_dirs, delete_blueprint, get_blueprint, list_blueprints, project_blueprints_dir,
    save_blueprint, user_blueprints_dir, validate, BlueprintEntry, BlueprintScope, Bounds,
    Diagnostic, StoreError, StoredBlueprint, ValidateEnv,
};
use serde::Serialize;
use serde_json::Value;

use super::BridgeError;

fn store_error(e: StoreError) -> BridgeError {
    let message = e.to_string();
    match e {
        StoreError::NotFound => BridgeError::new("not_found", message),
        StoreError::Conflict { current } => BridgeError::new("conflict", message)
            .with_details(serde_json::json!({ "current": current })),
        StoreError::Invalid(_) => BridgeError::new("invalid", message),
        StoreError::Io(_) => BridgeError::new("io", message),
    }
}

/// An existing absolute project directory, or a `bad_request`.
fn project_dir(project: Option<&str>) -> Result<Option<PathBuf>, BridgeError> {
    let Some(project) = project.filter(|p| !p.is_empty()) else {
        return Ok(None);
    };
    let path = Path::new(project);
    if !path.is_absolute() || !path.is_dir() {
        return Err(BridgeError::bad_request(format!(
            "the project must be an existing absolute directory, got {project:?}"
        )));
    }
    Ok(Some(path.to_path_buf()))
}

/// The directory holding blueprints of `scope`.
fn scope_dir(scope: BlueprintScope, project: Option<&str>) -> Result<PathBuf, BridgeError> {
    match scope {
        BlueprintScope::User => Ok(user_blueprints_dir()),
        BlueprintScope::Project => match project_dir(project)? {
            Some(p) => Ok(project_blueprints_dir(&p)),
            None => Err(BridgeError::bad_request(
                "a project blueprint needs the project directory",
            )),
        },
        _ => Err(BridgeError::bad_request(
            "only user and project blueprints are files",
        )),
    }
}

/// Run blocking file work off the async threads.
async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, BridgeError> + Send + 'static,
) -> Result<T, BridgeError> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| BridgeError::internal(e.to_string()))?
}

fn env() -> ValidateEnv {
    crate::settings::blueprint_validate_env()
}

/// `blueprint_validate`'s answer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ValidationOut {
    pub rev: String,
    /// No errors: the document can be started.
    pub runnable: bool,
    pub diagnostics: Vec<Diagnostic>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bounds: Option<Bounds>,
}

pub fn validate_doc(doc: &Value, env: &ValidateEnv) -> ValidationOut {
    let v = validate(doc, env);
    ValidationOut {
        runnable: !v.has_errors(),
        rev: v.rev,
        diagnostics: v.diagnostics,
        bounds: v.bounds,
    }
}

#[tauri::command]
pub async fn blueprints_list(project: Option<String>) -> Result<Vec<BlueprintEntry>, BridgeError> {
    blocking(move || {
        let project = project_dir(project.as_deref())?;
        Ok(list_blueprints(&blueprint_dirs(project.as_deref()), &env()))
    })
    .await
}

#[tauri::command]
pub async fn blueprint_get(
    scope: BlueprintScope,
    name: String,
    project: Option<String>,
) -> Result<StoredBlueprint, BridgeError> {
    blocking(move || {
        let dir = scope_dir(scope, project.as_deref())?;
        get_blueprint(scope, &dir, &name, &env()).map_err(store_error)
    })
    .await
}

/// Save `doc` as `<name>.json`. `base_rev` = the revision the edit started from; `None`
/// creates (a `conflict` if the file already exists).
#[tauri::command]
pub async fn blueprint_save(
    scope: BlueprintScope,
    name: String,
    doc: Value,
    base_rev: Option<String>,
    project: Option<String>,
) -> Result<StoredBlueprint, BridgeError> {
    blocking(move || {
        let dir = scope_dir(scope, project.as_deref())?;
        save_blueprint(scope, &dir, &name, &doc, base_rev.as_deref(), &env()).map_err(store_error)
    })
    .await
}

#[tauri::command]
pub async fn blueprint_delete(
    scope: BlueprintScope,
    name: String,
    base_rev: String,
    project: Option<String>,
) -> Result<(), BridgeError> {
    blocking(move || {
        let dir = scope_dir(scope, project.as_deref())?;
        delete_blueprint(&dir, &name, &base_rev).map_err(store_error)
    })
    .await
}

#[tauri::command]
pub async fn blueprint_validate(doc: Value) -> Result<ValidationOut, BridgeError> {
    blocking(move || Ok(validate_doc(&doc, &env()))).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stale_save_is_a_conflict_carrying_the_current_revision() {
        let err = store_error(StoreError::Conflict {
            current: Some("b1-abc".into()),
        });
        assert_eq!(err.code, "conflict");
        assert_eq!(
            err.details,
            Some(serde_json::json!({ "current": "b1-abc" }))
        );
        assert_eq!(store_error(StoreError::NotFound).code, "not_found");
    }

    #[test]
    fn only_file_scopes_have_a_directory() {
        assert!(scope_dir(BlueprintScope::User, None).is_ok());
        assert_eq!(
            scope_dir(BlueprintScope::Project, None).unwrap_err().code,
            "bad_request"
        );
        assert_eq!(
            scope_dir(BlueprintScope::Project, Some("relative/dir"))
                .unwrap_err()
                .code,
            "bad_request"
        );
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            scope_dir(BlueprintScope::Project, tmp.path().to_str()).unwrap(),
            tmp.path().join(".agentpit").join("blueprints")
        );
        assert!(scope_dir(BlueprintScope::Inline, None).is_err());
    }

    #[test]
    fn unknown_keys_round_trip_through_save_and_get() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("bp");
        let doc = serde_json::json!({
            "schema": "agentpit.blueprint/1",
            "name": "tiny",
            "x_studio": {"layout": {"a": [10, 20]}},
            "nodes": [{"id": "a", "kind": "agent", "task": "hi", "x_color": "teal"}],
            "edges": []
        });
        let env = ValidateEnv::default();
        let saved = save_blueprint(BlueprintScope::User, &dir, "tiny", &doc, None, &env).unwrap();
        let got = get_blueprint(BlueprintScope::User, &dir, "tiny", &env).unwrap();
        assert_eq!(got.doc, doc);
        assert_eq!(got.entry.rev, saved.entry.rev);
        // A second writer's stale save is refused.
        let stale = save_blueprint(
            BlueprintScope::User,
            &dir,
            "tiny",
            &doc,
            Some("b1-0000000000000000"),
            &env,
        )
        .map_err(store_error)
        .unwrap_err();
        assert_eq!(stale.code, "conflict");
    }
}
