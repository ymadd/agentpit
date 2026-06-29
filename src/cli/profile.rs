//! `agentpit profile show | seed [--force] | reset` — inspect and (re)seed the
//! machine-generated capability matrix in `profiles.toml`.
//!
//! - **show** renders the backend×category matrix (score / confidence / source).
//! - **seed** writes the hand-seeded priors, refusing to clobber an existing file unless
//!   `--force` is passed.
//! - **reset** clears every measured value, restoring the seeded priors.
//!
//! All three operations are additive to the public CLI surface and never touch the
//! hand-written `config.toml`.

use std::path::Path;

use anyhow::{Result, bail};
use clap::Subcommand;
use console::style;

use crate::profile::{ProfileSet, load_profiles, profiles_path, save_profiles, seeded_profiles};

#[derive(Subcommand, Debug)]
pub enum Action {
    /// Print the capability matrix (backend × category: score / confidence / source).
    Show,
    /// Write the hand-seeded priors to profiles.toml (refuses to clobber unless --force).
    Seed {
        /// Overwrite an existing profiles.toml.
        #[arg(long)]
        force: bool,
    },
    /// Clear all measured values, restoring the seeded priors.
    Reset,
}

/// Entry point. A bare `agentpit profile` (no sub-action) defaults to `show`.
pub async fn run(action: Option<Action>) -> Result<()> {
    match action.unwrap_or(Action::Show) {
        Action::Show => show(),
        Action::Seed { force } => seed(&profiles_path(), force),
        Action::Reset => reset(&profiles_path()),
    }
}

fn show() -> Result<()> {
    let path = profiles_path();
    let set = load_profiles(None)?;
    let persisted = path.exists();
    print!("{}", render_show(&set, &path, persisted));
    Ok(())
}

/// Render the capability matrix as a per-backend section list. Pure: builds and returns a
/// fresh `String`, mutating nothing.
fn render_show(set: &ProfileSet, path: &Path, persisted: bool) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    if persisted {
        let _ = writeln!(out, "profiles: {}", path.display());
    } else {
        let _ = writeln!(
            out,
            "profiles: {} (seeded defaults — not yet written; run `agentpit profile seed`)",
            path.display()
        );
    }

    if set.is_empty() {
        let _ = writeln!(out, "\n(no profiles)");
        return out;
    }

    for (backend, profile) in set.iter() {
        let measured = profile
            .measured_at
            .as_deref()
            .map(|m| format!("  measured_at={m}"))
            .unwrap_or_default();
        let _ = writeln!(
            out,
            "\n[{}]  source={}{}",
            style(*backend).cyan(),
            profile.source,
            measured
        );

        if profile.scores.is_empty() {
            let _ = writeln!(out, "  (no scores)");
            continue;
        }

        let _ = writeln!(
            out,
            "  {:<18} {:>5}  {:>5}  {:>7}",
            "category", "score", "conf", "samples"
        );
        for (category, score) in &profile.scores {
            let _ = writeln!(
                out,
                "  {:<18} {:>5}  {:>5.2}  {:>7}",
                category.as_str(),
                score.value,
                score.confidence,
                score.samples
            );
        }
    }

    out
}

/// Write the seeded priors to `path`. Refuses to overwrite an existing file unless `force`.
fn seed(path: &Path, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!(
            "{} already exists; pass --force to overwrite",
            path.display()
        );
    }
    let set = seeded_profiles();
    save_profiles(&set, path)?;
    println!(
        "seeded {} profiles → {}",
        set.len(),
        style(path.display()).green()
    );
    Ok(())
}

/// Clear measured values by rewriting the seeded priors. The seed set carries zero samples
/// and `source = seeded`, so this wipes any benchmarked/learned readings back to the baseline.
fn reset(path: &Path) -> Result<()> {
    let set = seeded_profiles();
    save_profiles(&set, path)?;
    println!(
        "reset capability profiles to seeded priors → {}",
        style(path.display()).green()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{
        CapabilityProfile, ProfileSource, Score, TaskCategory, load_profiles,
    };
    use crate::types::BackendId;
    use tempfile::tempdir;

    #[test]
    fn render_show_includes_source_and_scores() {
        let set = seeded_profiles();
        let path = Path::new("/tmp/profiles.toml");
        let out = render_show(&set, path, true);

        assert!(out.contains("profiles: /tmp/profiles.toml"));
        assert!(out.contains("source=seeded"));
        assert!(out.contains("coding"));
        // Claude's seeded coding score.
        assert!(out.contains("88"));
        // The seed confidence renders to two decimals.
        assert!(out.contains("0.40"));
    }

    #[test]
    fn render_show_flags_unpersisted_defaults() {
        let set = seeded_profiles();
        let out = render_show(&set, Path::new("/tmp/absent.toml"), false);
        assert!(out.contains("seeded defaults"));
        assert!(out.contains("profile seed"));
    }

    #[test]
    fn seed_writes_then_refuses_without_force() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("profiles.toml");

        seed(&path, false).unwrap();
        assert!(path.exists());

        // A second seed without --force must refuse rather than clobber.
        let err = seed(&path, false).unwrap_err();
        assert!(format!("{err:#}").contains("--force"), "got: {err:#}");

        // With --force it succeeds.
        seed(&path, true).unwrap();
    }

    #[test]
    fn reset_clears_measured_values_back_to_seeded() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("profiles.toml");

        // Seed a benchmarked profile with real samples on disk.
        let mut profile = CapabilityProfile::seeded(BackendId::Codex);
        profile.source = ProfileSource::Benchmarked;
        profile.scores.insert(
            TaskCategory::Review,
            Score {
                value: 91,
                samples: 24,
                confidence: 0.82,
            },
        );
        let measured = ProfileSet::from_profiles([profile]);
        save_profiles(&measured, &path).unwrap();

        reset(&path).unwrap();

        let reloaded = load_profiles(Some(&path)).unwrap();
        let codex = reloaded.get(BackendId::Codex).expect("codex re-seeded");
        assert_eq!(codex.source, ProfileSource::Seeded);
        // Every score is back to a zero-sample seeded prior.
        for score in codex.scores.values() {
            assert_eq!(score.samples, 0);
        }
    }
}
