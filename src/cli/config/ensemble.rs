use anyhow::{Result, anyhow};
use console::style;

use super::EnsembleTarget;
use crate::config::{load_config, save_config};
use crate::dispatch::build_registries;
use crate::types::BackendId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Aggregator {
    None,
    Backend(BackendId),
}

pub async fn run(target: EnsembleTarget) -> Result<()> {
    let mut loaded = load_config(None)?;
    let regs = build_registries(&loaded.config);
    let mut available: Vec<BackendId> = regs.available().into_iter().collect();
    available.sort();
    if available.is_empty() {
        anyhow::bail!("no backends registered");
    }

    let (current_members, current_aggregator) = match target {
        EnsembleTarget::Default => (
            loaded.config.ensemble.default_members.clone(),
            loaded.config.ensemble.aggregator,
        ),
        EnsembleTarget::Review => (
            loaded.config.ensemble.review_members.clone(),
            loaded.config.ensemble.review_aggregator,
        ),
    };

    cliclack::intro(style(format!(" ensemble: {} ", target.as_str())).on_cyan().black())
        .map_err(|e| anyhow!("intro failed: {e}"))?;

    let initial_members: Vec<BackendId> = current_members
        .iter()
        .copied()
        .filter(|b| available.contains(b))
        .collect();
    let mut ms = cliclack::multiselect(format!("Members for ensemble.{}", target.as_str()))
        .required(true)
        .initial_values(initial_members);
    for b in &available {
        ms = ms.item(*b, b.to_string(), "");
    }
    let members: Vec<BackendId> = ms
        .interact()
        .map_err(|e| anyhow!("multiselect failed: {e}"))?;

    let mut agg_sel = cliclack::select(format!("Aggregator for ensemble.{}", target.as_str()))
        .item(Aggregator::None, "(none)", "no aggregator; concatenate members");
    for b in &available {
        agg_sel = agg_sel.item(Aggregator::Backend(*b), b.to_string(), "");
    }
    let initial = current_aggregator
        .filter(|b| available.contains(b))
        .map(Aggregator::Backend)
        .unwrap_or(Aggregator::None);
    agg_sel = agg_sel.initial_value(initial);
    let aggregator = agg_sel
        .interact()
        .map_err(|e| anyhow!("select failed: {e}"))?;
    let aggregator = match aggregator {
        Aggregator::None => None,
        Aggregator::Backend(b) => Some(b),
    };

    match target {
        EnsembleTarget::Default => {
            loaded.config.ensemble.default_members = members.clone();
            loaded.config.ensemble.aggregator = aggregator;
        }
        EnsembleTarget::Review => {
            loaded.config.ensemble.review_members = members.clone();
            loaded.config.ensemble.review_aggregator = aggregator;
        }
    }

    let path = save_config(&loaded.config)?;
    let summary = format!(
        "members: {}\naggregator: {}",
        members
            .iter()
            .map(BackendId::to_string)
            .collect::<Vec<_>>()
            .join(", "),
        aggregator
            .map(|b| b.to_string())
            .unwrap_or_else(|| "(none)".into()),
    );
    cliclack::outro_note(
        format!("Saved ensemble.{} to {}", target.as_str(), path.display()),
        summary,
    )
    .map_err(|e| anyhow!("outro failed: {e}"))?;
    Ok(())
}
