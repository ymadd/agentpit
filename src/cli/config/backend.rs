use anyhow::{Result, anyhow};
use console::style;

use crate::cli::cancel::{self, Nav};
use crate::config::{BackendOverride, load_config, save_config};
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

    let backend = loaded
        .config
        .backends
        .entry(id)
        .or_insert_with(BackendOverride::default);
    backend.transport = Some(transport);
    backend.model = model.clone();

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

    cliclack::outro(format!(
        "Saved to {}",
        style(path.display().to_string()).dim(),
    ))
    .map_err(|e| anyhow!("outro failed: {e}"))?;
    Ok(())
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
