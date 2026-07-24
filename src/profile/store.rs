//! `profiles.toml` persistence: load the machine-generated capability matrix, seed it on
//! first run, and save it atomically.
//!
//! Kept deliberately separate from `config.rs`: `config.toml` is hand-written (`[routes]`
//! etc.) while `profiles.toml` is machine-generated (seeded → benchmarked → learned). The
//! split stops a benchmark run from ever clobbering a user's hand-tuned config.
//!
//! Wire shape (design §1.4): a single `[profile.<backend>]` table per backend.
//!
//! ```toml
//! [profile.claude]
//! source = "seeded"
//!
//! [profile.claude.scores.coding]
//! value = 88
//! samples = 0
//! confidence = 0.4
//! ```

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use super::ProfileSet;
use super::category::TaskCategory;
use super::model::{CapabilityProfile, ProfileSource, Score, TelemetryStats};
use super::seed::seeded_profiles;
use crate::config::xdg_config_home;
use crate::types::BackendId;

/// Path to `profiles.toml` under the XDG config home (`~/.config/agentpit/profiles.toml`).
/// Mirrors `config::default_config_path` so the two files sit side by side.
pub fn profiles_path() -> PathBuf {
    xdg_config_home().join("agentpit").join("profiles.toml")
}

/// One backend's row on the wire. The backend itself is the map key, so it is not repeated
/// here. Scalar fields (`source`, `measured_at`) precede the table fields (`scores`,
/// `telemetry`) so the TOML serializer never has to emit a value after a table.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProfileEntry {
    source: ProfileSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    measured_at: Option<String>,
    #[serde(default)]
    scores: BTreeMap<TaskCategory, WireScore>,
    #[serde(default, skip_serializing_if = "telemetry_is_empty")]
    telemetry: TelemetryStats,
}

/// One cell on the wire. `source` is optional because files written before per-cell
/// provenance existed only carried the profile-level `source`; [`WireScore::resolve`]
/// migrates those on load.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct WireScore {
    value: u8,
    samples: u16,
    confidence: f32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    source: Option<ProfileSource>,
}

impl WireScore {
    fn from_score(score: &Score) -> Self {
        Self {
            value: score.value,
            samples: score.samples,
            confidence: score.confidence,
            source: Some(score.source),
        }
    }

    /// Migration for pre-per-cell files: a cell with samples inherits the profile-level
    /// source (that measurement is what set the profile's source in the first place); a
    /// zero-sample cell is an untouched seeded prior regardless of the profile's source —
    /// the same distinction the old `profile show` src column drew.
    fn resolve(self, entry_source: ProfileSource) -> Score {
        let source = self.source.unwrap_or(if self.samples > 0 {
            entry_source
        } else {
            ProfileSource::Seeded
        });
        Score {
            value: self.value,
            samples: self.samples,
            confidence: self.confidence,
            source,
        }
    }
}

/// Whole-file wire shape: a table of per-backend entries keyed by backend name.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ProfilesFile {
    #[serde(default)]
    profile: BTreeMap<BackendId, ProfileEntry>,
}

/// True when telemetry carries no observations — lets `save` omit an empty `[telemetry]`
/// table rather than writing `{}`.
fn telemetry_is_empty(t: &TelemetryStats) -> bool {
    *t == TelemetryStats::default()
}

impl ProfilesFile {
    /// Project a `ProfileSet` into the wire shape. Pure — the input is only read.
    fn from_set(set: &ProfileSet) -> Self {
        let profile = set
            .iter()
            .map(|(backend, p)| {
                (
                    *backend,
                    ProfileEntry {
                        source: p.source,
                        measured_at: p.measured_at.clone(),
                        scores: p
                            .scores
                            .iter()
                            .map(|(category, score)| (*category, WireScore::from_score(score)))
                            .collect(),
                        telemetry: p.telemetry.clone(),
                    },
                )
            })
            .collect();
        Self { profile }
    }

    /// Reconstruct a `ProfileSet`, stitching each entry's backend key back into its profile.
    fn into_set(self) -> ProfileSet {
        let profiles = self
            .profile
            .into_iter()
            .map(|(backend, entry)| CapabilityProfile {
                backend,
                scores: entry
                    .scores
                    .into_iter()
                    .map(|(category, wire)| (category, wire.resolve(entry.source)))
                    .collect(),
                telemetry: entry.telemetry,
                source: entry.source,
                measured_at: entry.measured_at,
            });
        ProfileSet::from_profiles(profiles)
    }
}

/// Load the capability profiles from `profiles.toml`.
///
/// - `override_path` lets tests point at a scratch file; `None` uses [`profiles_path`].
/// - A missing file is not an error: the hand-seeded matrix ([`seeded_profiles`]) is
///   returned so first-run routing has priors to work with.
/// - A malformed file is an error (we never silently fall back over corruption — that would
///   mask a benchmark run that wrote garbage).
pub fn load_profiles(override_path: Option<&Path>) -> Result<ProfileSet> {
    let path = override_path
        .map(PathBuf::from)
        .unwrap_or_else(profiles_path);

    match fs::read_to_string(&path) {
        Ok(raw) => {
            let file: ProfilesFile = toml::from_str(&raw)
                .with_context(|| format!("Failed to load {}", path.display()))?;
            Ok(file.into_set())
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(seeded_profiles()),
        Err(err) => Err(anyhow!("Failed to read {}: {err}", path.display())),
    }
}

/// Save the capability profiles to `path` atomically.
///
/// Writes to a sibling temp file and renames it into place, so a reader never sees a
/// half-written `profiles.toml` and a crash mid-write leaves the previous file intact.
/// Follows the directory-creation style of `config::save_config_at`.
pub fn save_profiles(set: &ProfileSet, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let raw = toml::to_string_pretty(&ProfilesFile::from_set(set))
        .with_context(|| format!("failed to serialize profiles for {}", path.display()))?;

    let tmp = temp_sibling(path);
    fs::write(&tmp, raw).with_context(|| format!("failed to write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| {
        // Best-effort cleanup; ignore failure since the original temp error is what matters.
        let _ = fs::remove_file(&tmp);
        format!("failed to move {} into {}", tmp.display(), path.display())
    })?;
    Ok(())
}

/// A temp path next to `path` (same directory → same filesystem → atomic rename). The pid
/// suffix keeps concurrent writers from colliding on the same temp name.
fn temp_sibling(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(format!(".tmp.{}", std::process::id()));
    match path.parent() {
        Some(parent) => parent.join(name),
        None => PathBuf::from(name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tempfile::tempdir;

    fn available() -> HashSet<BackendId> {
        BackendId::ALL.iter().copied().collect()
    }

    #[test]
    fn missing_file_returns_seeded() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("absent.toml");
        let set = load_profiles(Some(&path)).unwrap();

        // Same shape as the seed: five known backends, all Seeded.
        assert_eq!(set.len(), 5);
        let profile = set.get(BackendId::Claude).expect("claude seeded");
        assert_eq!(profile.source, ProfileSource::Seeded);
        assert_eq!(
            set.best_for(TaskCategory::Coding, &available()).unwrap().0,
            BackendId::Claude
        );
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("profiles.toml");

        let original = seeded_profiles();
        save_profiles(&original, &path).unwrap();
        assert!(path.exists(), "save should create the file");

        let reloaded = load_profiles(Some(&path)).unwrap();

        assert_eq!(reloaded.len(), original.len());
        for (backend, profile) in original.iter() {
            let got = reloaded.get(*backend).expect("backend survives round-trip");
            assert_eq!(got, profile, "profile for {backend:?} differs after reload");
        }
    }

    #[test]
    fn round_trips_benchmarked_scores_and_telemetry() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("profiles.toml");

        let mut profile = CapabilityProfile::seeded(BackendId::Codex);
        profile.source = ProfileSource::Benchmarked;
        profile.measured_at = Some("2026-06-30T00:00:00Z".into());
        profile.scores.insert(
            TaskCategory::Review,
            Score {
                value: 91,
                samples: 24,
                confidence: 0.82,
                source: ProfileSource::Benchmarked,
            },
        );
        profile.telemetry = TelemetryStats {
            success: Some(18),
            total: Some(20),
            p50_ms: Some(1200),
            p95_ms: Some(4300),
        };
        let set = ProfileSet::from_profiles([profile.clone()]);

        save_profiles(&set, &path).unwrap();
        let reloaded = load_profiles(Some(&path)).unwrap();

        assert_eq!(reloaded.get(BackendId::Codex).unwrap(), &profile);
    }

    #[test]
    fn save_creates_missing_parent_dirs() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("deep").join("profiles.toml");
        save_profiles(&seeded_profiles(), &path).unwrap();
        assert!(path.exists());
        // No leftover temp file in the directory.
        let leftovers: Vec<_> = fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "temp file should be renamed away");
    }

    /// Files written before per-cell provenance carry only the profile-level `source`.
    /// On load, measured cells (samples > 0) inherit it; zero-sample cells stay seeded.
    #[test]
    fn legacy_file_without_cell_source_migrates_on_load() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("legacy.toml");
        fs::write(
            &path,
            r#"
[profile.codex]
source = "benchmarked"

[profile.codex.scores.review]
value = 91
samples = 24
confidence = 0.82

[profile.codex.scores.coding]
value = 60
samples = 0
confidence = 0.4
"#,
        )
        .unwrap();

        let set = load_profiles(Some(&path)).unwrap();
        let codex = set.get(BackendId::Codex).unwrap();
        assert_eq!(
            codex.score(TaskCategory::Review).unwrap().source,
            ProfileSource::Benchmarked
        );
        assert_eq!(
            codex.score(TaskCategory::Coding).unwrap().source,
            ProfileSource::Seeded
        );

        // Saving re-writes with explicit per-cell sources that round-trip unchanged.
        save_profiles(&set, &path).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("source"));
        let reloaded = load_profiles(Some(&path)).unwrap();
        assert_eq!(reloaded.get(BackendId::Codex).unwrap(), codex);
    }

    #[test]
    fn malformed_file_is_an_error() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("broken.toml");
        fs::write(&path, "this is = not [valid").unwrap();
        let err = load_profiles(Some(&path)).unwrap_err();
        assert!(
            format!("{err:#}").contains("Failed to load"),
            "got: {err:#}"
        );
    }
}
