//! Capability profiles: the backend×`TaskCategory` score matrix that drives diagnostic
//! routing. A `ProfileSet` answers "which available backend is best at this category?".

pub mod bench;
pub mod category;
pub mod learn;
pub mod model;
pub mod seed;
pub mod store;

use std::collections::{BTreeMap, HashSet};

pub use category::TaskCategory;
pub use model::{
    BenchmarkResult, CapabilityProfile, ProfileSource, Score, TelemetryStats, apply_benchmark,
    apply_learned,
};
pub use seed::seeded_profiles;
pub use store::{load_profiles, profiles_path, save_profiles};

use crate::types::BackendId;

/// The full set of capability profiles, keyed by backend. Holds the score matrix the
/// router consults; built immutably (insert returns nothing surprising — callers either
/// construct via `from_profiles` or fold in one profile at a time).
#[derive(Debug, Clone, Default)]
pub struct ProfileSet {
    profiles: BTreeMap<BackendId, CapabilityProfile>,
}

impl ProfileSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from an iterator of profiles (last one wins per backend).
    pub fn from_profiles<I>(profiles: I) -> Self
    where
        I: IntoIterator<Item = CapabilityProfile>,
    {
        let profiles = profiles.into_iter().map(|p| (p.backend, p)).collect();
        Self { profiles }
    }

    /// Insert/replace one backend's profile.
    pub fn insert(&mut self, profile: CapabilityProfile) {
        self.profiles.insert(profile.backend, profile);
    }

    pub fn get(&self, backend: BackendId) -> Option<&CapabilityProfile> {
        self.profiles.get(&backend)
    }

    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }

    pub fn len(&self) -> usize {
        self.profiles.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&BackendId, &CapabilityProfile)> {
        self.profiles.iter()
    }

    /// Argmax over the backends in `available` that have a score for `category`.
    ///
    /// Returns the highest-scoring `(backend, score)`, or `None` when no available backend
    /// has scored that category (including when `available` is empty). Ties break
    /// deterministically by confidence, then sample count, then backend order — never by
    /// hash-map iteration order, so the same inputs always pick the same backend.
    pub fn best_for(
        &self,
        category: TaskCategory,
        available: &HashSet<BackendId>,
    ) -> Option<(BackendId, Score)> {
        self.profiles
            .iter()
            .filter(|(backend, _)| available.contains(*backend))
            .filter_map(|(backend, profile)| profile.score(category).map(|score| (*backend, score)))
            .max_by(|(_, a), (_, b)| {
                a.value
                    .cmp(&b.value)
                    .then(a.confidence.total_cmp(&b.confidence))
                    .then(a.samples.cmp(&b.samples))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile_with(backend: BackendId, category: TaskCategory, value: u8) -> CapabilityProfile {
        let mut p = CapabilityProfile::seeded(backend);
        p.scores.insert(category, Score::seeded(value));
        p
    }

    fn available(backends: &[BackendId]) -> HashSet<BackendId> {
        backends.iter().copied().collect()
    }

    #[test]
    fn best_for_picks_argmax_within_available() {
        let set = ProfileSet::from_profiles([
            profile_with(BackendId::Claude, TaskCategory::Coding, 70),
            profile_with(BackendId::Codex, TaskCategory::Coding, 90),
            profile_with(BackendId::Gemini, TaskCategory::Coding, 80),
        ]);

        let (backend, score) = set
            .best_for(
                TaskCategory::Coding,
                &available(&[BackendId::Claude, BackendId::Codex, BackendId::Gemini]),
            )
            .unwrap();
        assert_eq!(backend, BackendId::Codex);
        assert_eq!(score.value, 90);
    }

    #[test]
    fn best_for_respects_availability_filter() {
        let set = ProfileSet::from_profiles([
            profile_with(BackendId::Claude, TaskCategory::Coding, 70),
            profile_with(BackendId::Codex, TaskCategory::Coding, 90),
        ]);

        // Codex is the global best but is not available — Claude wins.
        let (backend, score) = set
            .best_for(TaskCategory::Coding, &available(&[BackendId::Claude]))
            .unwrap();
        assert_eq!(backend, BackendId::Claude);
        assert_eq!(score.value, 70);
    }

    #[test]
    fn best_for_returns_none_when_available_is_empty() {
        let set =
            ProfileSet::from_profiles([profile_with(BackendId::Claude, TaskCategory::Coding, 70)]);
        assert!(
            set.best_for(TaskCategory::Coding, &available(&[]))
                .is_none()
        );
    }

    #[test]
    fn best_for_returns_none_when_no_backend_scored_category() {
        let set =
            ProfileSet::from_profiles([profile_with(BackendId::Claude, TaskCategory::Coding, 70)]);
        // Claude is available but has no Debug score.
        assert!(
            set.best_for(TaskCategory::Debug, &available(&[BackendId::Claude]))
                .is_none()
        );
    }
}
