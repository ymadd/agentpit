use anyhow::{Result, anyhow};
use console::style;

use super::EnsembleTarget;
use crate::cli::cancel::{self, Nav};
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

    // Capture prior values (immutable read) BEFORE any mutation for before→after display.
    let (prior_members, prior_aggregator) = match target {
        EnsembleTarget::Default => (
            loaded.config.ensemble.default_members.clone(),
            loaded.config.ensemble.aggregator,
        ),
        EnsembleTarget::Review => (
            loaded.config.ensemble.review_members.clone(),
            loaded.config.ensemble.review_aggregator,
        ),
        EnsembleTarget::SecurityReview => (
            loaded.config.ensemble.security_review_members.clone(),
            loaded.config.ensemble.security_review_aggregator,
        ),
        EnsembleTarget::Rescue => (
            loaded.config.ensemble.rescue_members.clone(),
            loaded.config.ensemble.rescue_aggregator,
        ),
        EnsembleTarget::Refactor => (
            loaded.config.ensemble.refactor_members.clone(),
            loaded.config.ensemble.refactor_aggregator,
        ),
    };

    cliclack::intro(
        style(format!(" ensemble: {} ", target.as_str()))
            .on_cyan()
            .black(),
    )
    .map_err(|e| anyhow!("intro failed: {e}"))?;

    // Pre-select only members that are currently available.
    let initial_members: Vec<BackendId> = prior_members
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

    // Route the multiselect through the cancel helper — Esc → clean return.
    let members: Vec<BackendId> = match cancel::prompt(ms.interact())? {
        Nav::Value(v) => v,
        Nav::Back => {
            cliclack::outro("(cancelled — no changes made)")
                .map_err(|e| anyhow!("outro failed: {e}"))?;
            return Ok(());
        }
    };

    let mut agg_sel = cliclack::select(format!("Aggregator for ensemble.{}", target.as_str()))
        .item(
            Aggregator::None,
            "(none)",
            "no aggregator; concatenate members",
        );
    for b in &available {
        agg_sel = agg_sel.item(Aggregator::Backend(*b), b.to_string(), "");
    }
    // Pre-select the currently configured aggregator when it is available.
    let initial_agg = prior_aggregator
        .filter(|b| available.contains(b))
        .map(Aggregator::Backend)
        .unwrap_or(Aggregator::None);
    agg_sel = agg_sel.initial_value(initial_agg);

    // Route the aggregator select through the cancel helper — Esc → clean return.
    let aggregator_raw = match cancel::prompt(agg_sel.interact())? {
        Nav::Value(v) => v,
        Nav::Back => {
            cliclack::outro("(cancelled — no changes made)")
                .map_err(|e| anyhow!("outro failed: {e}"))?;
            return Ok(());
        }
    };
    let aggregator: Option<BackendId> = match aggregator_raw {
        Aggregator::None => None,
        Aggregator::Backend(b) => Some(b),
    };

    // Apply the mutation.
    match target {
        EnsembleTarget::Default => {
            loaded.config.ensemble.default_members = members.clone();
            loaded.config.ensemble.aggregator = aggregator;
        }
        EnsembleTarget::Review => {
            loaded.config.ensemble.review_members = members.clone();
            loaded.config.ensemble.review_aggregator = aggregator;
        }
        EnsembleTarget::SecurityReview => {
            loaded.config.ensemble.security_review_members = members.clone();
            loaded.config.ensemble.security_review_aggregator = aggregator;
        }
        EnsembleTarget::Rescue => {
            loaded.config.ensemble.rescue_members = members.clone();
            loaded.config.ensemble.rescue_aggregator = aggregator;
        }
        EnsembleTarget::Refactor => {
            loaded.config.ensemble.refactor_members = members.clone();
            loaded.config.ensemble.refactor_aggregator = aggregator;
        }
    }

    let path = save_config(&loaded.config)?;

    // Uniform before→after confirmations via the shared helper.
    let prior_members_str = if prior_members.is_empty() {
        "(none)".into()
    } else {
        prior_members
            .iter()
            .map(BackendId::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let new_members_str = members
        .iter()
        .map(BackendId::to_string)
        .collect::<Vec<_>>()
        .join(", ");

    let prior_agg_str = prior_aggregator
        .map(|b| b.to_string())
        .unwrap_or_else(|| "(none)".into());
    let new_agg_str = aggregator
        .map(|b| b.to_string())
        .unwrap_or_else(|| "(none)".into());

    cancel::confirm_change(
        &format!("ensemble.{}.members", target.as_str()),
        &prior_members_str,
        &new_members_str,
    );
    cancel::confirm_change(
        &format!("ensemble.{}.aggregator", target.as_str()),
        &prior_agg_str,
        &new_agg_str,
    );

    cliclack::outro_note(
        format!(
            "Saved ensemble.{} to {}",
            target.as_str(),
            style(path.display().to_string()).dim()
        ),
        format!("members: {new_members_str}\naggregator: {new_agg_str}"),
    )
    .map_err(|e| anyhow!("outro failed: {e}"))?;
    Ok(())
}
