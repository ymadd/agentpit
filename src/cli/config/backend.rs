use anyhow::{Result, anyhow};
use console::style;

use crate::cli::cancel::{self, Nav};
use crate::config::{BackendOverride, load_config, save_config};
use crate::effort::Effort;
use crate::types::{BackendId, Transport};

pub async fn run(id: BackendId) -> Result<()> {
    let mut loaded = load_config(None)?;

    let current = loaded.config.backends.get(&id).and_then(|o| o.transport);
    let current_str = current
        .map(|t| t.as_str().to_string())
        .unwrap_or_else(|| "(default)".into());
    let current_model = loaded
        .config
        .backends
        .get(&id)
        .and_then(|o| o.model.as_deref());
    let current_model_str = current_model.unwrap_or("(CLI default)");
    let current_effort = loaded.config.backends.get(&id).and_then(|o| o.effort);
    let current_effort_str = current_effort
        .map(|e| e.to_string())
        .unwrap_or_else(|| "(CLI default)".into());

    cliclack::intro(style(format!(" backend: {id} ")).on_cyan().black())
        .map_err(|e| anyhow!("intro failed: {e}"))?;

    let transport = match cancel::prompt(
        cliclack::select(format!("Transport for {id}  (current: {current_str})"))
            .item(Transport::Exec, "exec", "spawn the CLI per request")
            .item(Transport::Acp, "acp", "persistent ACP session")
            .interact(),
    )? {
        Nav::Value(t) => t,
        Nav::Back => {
            cliclack::outro("(cancelled — no changes made)")
                .map_err(|e| anyhow!("outro failed: {e}"))?;
            return Ok(());
        }
    };

    let model_question = format!("Default model for {id}  (current: {current_model_str})");
    let mut model_prompt =
        cliclack::input(&model_question).placeholder("leave blank to use the backend CLI default");
    if let Some(model) = current_model {
        model_prompt = model_prompt.default_input(model);
    }
    let model = match cancel::prompt(model_prompt.interact())? {
        Nav::Value(model) => normalize_model(model),
        Nav::Back => {
            cliclack::outro("(cancelled — no changes made)")
                .map_err(|e| anyhow!("outro failed: {e}"))?;
            return Ok(());
        }
    };

    // Reasoning effort. "(CLI default)" is a real choice, not a placeholder: it leaves the
    // backend on its own default rather than pinning a rung.
    let mut effort_prompt = cliclack::select(format!(
        "Default reasoning effort for {id}  (current: {current_effort_str})"
    ))
    .item(
        None,
        "(CLI default)",
        "leave the backend CLI on its own default",
    );
    for e in Effort::ALL {
        effort_prompt = effort_prompt.item(Some(*e), e.as_str(), clamp_note(id, *e));
    }
    let effort = match cancel::prompt(effort_prompt.interact())? {
        Nav::Value(e) => e,
        Nav::Back => {
            cliclack::outro("(cancelled — no changes made)")
                .map_err(|e| anyhow!("outro failed: {e}"))?;
            return Ok(());
        }
    };

    // Capture the prior value (immutable read) before mutating state.
    let prior_str = loaded
        .config
        .backends
        .get(&id)
        .and_then(|o| o.transport)
        .map(|t| t.as_str().to_string())
        .unwrap_or_else(|| "(default)".into());
    let prior_model = loaded
        .config
        .backends
        .get(&id)
        .and_then(|o| o.model.clone());
    let prior_effort = loaded.config.backends.get(&id).and_then(|o| o.effort);

    let backend = loaded
        .config
        .backends
        .entry(id)
        .or_insert_with(BackendOverride::default);
    backend.transport = Some(transport);
    backend.model = model.clone();
    backend.effort = effort;

    let path = save_config(&loaded.config)?;

    cancel::confirm_change(
        &format!("backend.{id}.transport"),
        &prior_str,
        transport.as_str(),
    );
    cancel::confirm_change(
        &format!("backend.{id}.model"),
        prior_model.as_deref().unwrap_or("(CLI default)"),
        model.as_deref().unwrap_or("(CLI default)"),
    );

    cancel::confirm_change(
        &format!("backend.{id}.effort"),
        &prior_effort
            .map(|e| e.to_string())
            .unwrap_or_else(|| "(CLI default)".into()),
        &effort
            .map(|e| e.to_string())
            .unwrap_or_else(|| "(CLI default)".into()),
    );

    cliclack::outro(format!(
        "Saved to {}",
        style(path.display().to_string()).dim(),
    ))
    .map_err(|e| anyhow!("outro failed: {e}"))?;
    Ok(())
}

/// The per-rung hint shown in the picker: says so when this backend cannot express the rung and
/// will run it clamped, so the choice is not silently downgraded behind the user's back.
fn clamp_note(id: BackendId, e: Effort) -> String {
    match e.clamp_for(id) {
        clamped if clamped == e => String::new(),
        clamped => format!("{id} runs this as {clamped}"),
    }
}

fn normalize_model(model: String) -> Option<String> {
    let model = model.trim();
    (!model.is_empty()).then(|| model.to_string())
}

#[cfg(test)]
mod tests {
    use super::normalize_model;

    #[test]
    fn blank_model_uses_cli_default_and_nonblank_is_trimmed() {
        assert_eq!(normalize_model("   ".into()), None);
        assert_eq!(
            normalize_model("  opencode/big-pickle  ".into()).as_deref(),
            Some("opencode/big-pickle")
        );
    }
}
