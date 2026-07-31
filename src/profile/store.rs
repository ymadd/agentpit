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

use super::category::TaskCategory;
use super::model::{CapabilityProfile, ProfileSource, Score, TelemetryStats};
use super::seed::seeded_profiles;
use super::{ProfileKey, ProfileSet};
use crate::config::xdg_config_home;
use crate::effort::Effort;
use crate::types::BackendId;

/// Path to `profiles.toml` under the XDG config home (`~/.config/agentpit/profiles.toml`).
/// Mirrors `config::default_config_path` so the two files sit side by side.
pub fn profiles_path() -> PathBuf {
    xdg_config_home().join("agentpit").join("profiles.toml")
}

/// One row on the wire. Scalar fields (`source`, `measured_at`, `model`, `effort`) precede the
/// table fields (`scores`, `telemetry`) so the TOML serializer never has to emit a value after a
/// table.
///
/// The map key names the backend and, for a measured variant, decorates it with the variant —
/// `codex` vs `codex@gpt-5.4-codex/xhigh`. The DECORATION IS COSMETIC: `model` / `effort` inside
/// the entry are authoritative, so a model id containing any character at all round-trips
/// without an escaping scheme. Only the part before the first `@` is parsed back.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ProfileEntry {
    source: ProfileSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    measured_at: Option<String>,
    /// What this row is about. Absent in every file written before the effort ladder existed,
    /// which is why both are `#[serde(default)]` — an old file loads as the unpinned row it
    /// always was, and routing is unchanged for anyone who never pins anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    effort: Option<Effort>,
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

    /// Migration for pre-per-cell files, which recorded provenance only per profile.
    ///
    /// - A zero-sample cell is an untouched seeded prior regardless of the profile's source.
    /// - A measured cell in a `Seeded`/`Learned` profile takes that source directly.
    /// - A measured cell in a `Benchmarked` profile is **ambiguous** and is migrated as
    ///   `Learned`, not `Benchmarked`. Under the old profile-level gate a `learn` fold could
    ///   write cells and a later *partial* benchmark then promoted the whole profile, so such
    ///   a file genuinely mixes learned and benchmarked cells with nothing to tell them
    ///   apart. The two ways to be wrong are not symmetric: labelling a learned cell
    ///   `Benchmarked` freezes it against every future fold (the exact defect per-cell
    ///   provenance exists to remove, and unrecoverable without a hand edit), while labelling
    ///   a benchmarked cell `Learned` merely lets telemetry update it, and `profile run`
    ///   restores the measurement. So the ambiguous case resolves to the recoverable side.
    ///
    /// Every save from here on records the real per-cell source, so this runs once per file.
    fn resolve(self, entry_source: ProfileSource) -> Score {
        let source = self.source.unwrap_or(match (self.samples, entry_source) {
            (0, _) => ProfileSource::Seeded,
            (_, ProfileSource::Benchmarked) => ProfileSource::Learned,
            (_, source) => source,
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
///
/// The key is a plain `String`, not a `BackendId`, so that a profile for a backend this
/// build no longer knows is skipped instead of failing the whole load. `profiles.toml` is
/// machine-generated and outlives the backend list — a file written when `gemini` existed
/// must still load after that backend is removed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct ProfilesFile {
    #[serde(default)]
    profile: BTreeMap<String, ProfileEntry>,
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
            .map(|(key, p)| {
                (
                    wire_key(key),
                    ProfileEntry {
                        source: p.source,
                        measured_at: p.measured_at.clone(),
                        scores: p
                            .scores
                            .iter()
                            .map(|(category, score)| (*category, WireScore::from_score(score)))
                            .collect(),
                        telemetry: p.telemetry.clone(),
                        model: p.model.clone(),
                        effort: p.effort,
                    },
                )
            })
            .collect();
        Self { profile }
    }

    /// Reconstruct a `ProfileSet`, stitching each entry's backend key back into its profile.
    /// Entries naming a backend this build does not support are dropped (see the struct doc).
    fn into_set(self) -> ProfileSet {
        let profiles = self
            .profile
            .into_iter()
            .filter_map(|(name, entry)| Some((backend_of(&name)?, entry)))
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
                model: entry.model,
                effort: entry.effort,
            });
        ProfileSet::from_profiles(profiles)
    }
}

/// The TOML table name for one row: the bare backend for an unpinned row (byte-identical to
/// every file written before variants existed), else `<backend>@<model>/<effort>`.
fn wire_key(key: &ProfileKey) -> String {
    match key.is_unpinned() {
        true => key.backend.to_string(),
        false => format!("{}@{}", key.backend, key.variant_label()),
    }
}

/// The backend a wire key names: everything before the first `@`. The variant decoration is not
/// parsed — the entry's own `model`/`effort` fields carry that.
fn backend_of(name: &str) -> Option<BackendId> {
    name.split('@').next()?.parse::<BackendId>().ok()
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
        assert_eq!(set.len(), 4);
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
        for (key, profile) in original.iter() {
            let got = reloaded
                .resolve(key.backend, key.model.as_deref(), key.effort)
                .expect("row survives round-trip");
            assert_eq!(got, profile, "profile for {key:?} differs after reload");
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

    /// Variant rows survive the TOML round-trip, including a model id full of characters that
    /// would break any separator-based key encoding — the key decoration is cosmetic, the
    /// entry's own `model`/`effort` fields are what is read back.
    #[test]
    fn round_trips_variant_rows_keyed_by_model_and_effort() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("profiles.toml");

        let hostile = "cloudflare-ai-gateway/workers-ai/@cf/zai-org/glm-5.2";
        let cell = |value: u8| Score {
            value,
            samples: 3,
            confidence: 0.6,
            source: ProfileSource::Benchmarked,
        };
        let mut low = CapabilityProfile::for_variant(
            BackendId::Opencode,
            Some(hostile.into()),
            Some(Effort::Low),
        );
        low.source = ProfileSource::Benchmarked;
        low.scores.insert(TaskCategory::Coding, cell(41));
        let mut max = CapabilityProfile::for_variant(
            BackendId::Opencode,
            Some(hostile.into()),
            Some(Effort::Max),
        );
        max.source = ProfileSource::Benchmarked;
        max.scores.insert(TaskCategory::Coding, cell(88));
        let unpinned = CapabilityProfile::seeded(BackendId::Opencode);

        let set = ProfileSet::from_profiles([low.clone(), max.clone(), unpinned.clone()]);
        save_profiles(&set, &path).unwrap();
        let reloaded = load_profiles(Some(&path)).unwrap();

        assert_eq!(reloaded.len(), 3, "three rows for one backend");
        assert_eq!(
            reloaded
                .resolve(BackendId::Opencode, Some(hostile), Some(Effort::Low))
                .unwrap(),
            &low
        );
        assert_eq!(
            reloaded
                .resolve(BackendId::Opencode, Some(hostile), Some(Effort::Max))
                .unwrap(),
            &max
        );
        assert_eq!(reloaded.get(BackendId::Opencode).unwrap(), &unpinned);
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
    ///
    /// Review finding (2026-07-25): migrating a measured cell in a *benchmarked* profile
    /// straight to `Benchmarked` re-created the very freeze per-cell provenance removes —
    /// a legacy `learn`-then-partial-benchmark file mixes learned and benchmarked cells
    /// indistinguishably, and the learned ones would never accept a fold again. The
    /// ambiguous case now resolves to `Learned`, which telemetry (and `profile run`) can
    /// still move.
    #[test]
    fn legacy_file_without_cell_source_migrates_to_the_recoverable_side() {
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

[profile.claude]
source = "learned"

[profile.claude.scores.docs]
value = 77
samples = 8
confidence = 0.6

# A backend this build no longer supports: the entry is skipped, not a load error.
[profile.gemini]
source = "seeded"

[profile.gemini.scores.coding]
value = 72
samples = 0
confidence = 0.4
"#,
        )
        .unwrap();

        let set = load_profiles(Some(&path)).unwrap();
        let codex = set.get(BackendId::Codex).unwrap();
        // Ambiguous (measured cell under a benchmarked profile) → Learned, so a later fold
        // is not locked out.
        assert_eq!(
            codex.score(TaskCategory::Review).unwrap().source,
            ProfileSource::Learned
        );
        // Never measured → still a seeded prior.
        assert_eq!(
            codex.score(TaskCategory::Coding).unwrap().source,
            ProfileSource::Seeded
        );
        // Unambiguous profile-level source is taken as-is.
        assert_eq!(
            set.get(BackendId::Claude)
                .unwrap()
                .score(TaskCategory::Docs)
                .unwrap()
                .source,
            ProfileSource::Learned
        );
        // The retired backend's entry was dropped rather than failing the whole load.
        assert_eq!(set.len(), 2, "only supported backends survive the load");

        // A learned fold can still update the migrated cell — the freeze is gone.
        let merged = crate::profile::apply_learned(
            codex,
            &[(
                TaskCategory::Review,
                Score {
                    value: 40,
                    samples: 12,
                    confidence: 0.7,
                    source: ProfileSource::Learned,
                },
            )]
            .into(),
        );
        assert_eq!(merged.score(TaskCategory::Review).unwrap().value, 40);

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
