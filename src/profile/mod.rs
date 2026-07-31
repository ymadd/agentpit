//! Capability profiles: the backend×`TaskCategory` score matrix that drives diagnostic
//! routing. A `ProfileSet` answers "which available backend is best at this category?".

pub mod bench;
pub mod category;
pub mod learn;
pub mod model;
pub mod seed;
pub mod status;
pub mod store;

use std::collections::{BTreeMap, HashSet};

pub use category::TaskCategory;
pub use model::{
    BenchmarkResult, CapabilityProfile, ProfileSource, Score, TelemetryStats, apply_benchmark,
    apply_learned,
};
pub use seed::seeded_profiles;
pub use store::{load_profiles, profiles_path, save_profiles};

use crate::effort::Effort;
use crate::types::BackendId;

/// What one capability row is ABOUT: a backend running a particular model at a particular
/// reasoning effort.
///
/// A capability score is not a property of a CLI's name — it is a property of harness + model +
/// effort, and those move the number a lot. Keying rows by the triple is what lets a `high` and a
/// `low` measurement of the same backend coexist instead of one silently overwriting the other.
///
/// `model: None` / `effort: None` mean "unpinned": the row describes the backend on whatever its
/// CLI defaults to. The hand-seeded priors are all unpinned, and an unpinned row is the fallback
/// when no exact variant has been measured (see [`ProfileSet::resolve`]).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ProfileKey {
    pub backend: BackendId,
    pub model: Option<String>,
    pub effort: Option<Effort>,
}

impl ProfileKey {
    /// The unpinned row for `backend` — what every seeded prior is keyed as.
    pub fn unpinned(backend: BackendId) -> Self {
        Self {
            backend,
            model: None,
            effort: None,
        }
    }

    pub fn new(backend: BackendId, model: Option<String>, effort: Option<Effort>) -> Self {
        Self {
            backend,
            model,
            effort,
        }
    }

    /// True when neither a model nor an effort is pinned.
    pub fn is_unpinned(&self) -> bool {
        self.model.is_none() && self.effort.is_none()
    }

    /// Human-readable variant label for display, e.g. `gpt-5.4-codex/xhigh`. Empty for an
    /// unpinned row.
    pub fn variant_label(&self) -> String {
        if self.is_unpinned() {
            return String::new();
        }
        format!(
            "{}/{}",
            self.model.as_deref().unwrap_or("*"),
            self.effort.map(|e| e.to_string()).unwrap_or("*".into())
        )
    }
}

/// The effective `(model, effort)` each backend would be dispatched at, derived from
/// `[backends.<id>]`. Routing scores a backend at the variant it would ACTUALLY run, so the
/// pins have to reach [`ProfileSet::resolve`] — otherwise a `max`-effort measurement would be
/// used to route a dispatch that runs at the CLI default.
#[derive(Debug, Clone, Default)]
pub struct Pins(BTreeMap<BackendId, (Option<String>, Option<Effort>)>);

impl Pins {
    /// Read the per-backend defaults out of the loaded config.
    pub fn from_config(config: &crate::config::HubConfig) -> Self {
        Self(
            config
                .backends
                .iter()
                .map(|(backend, o)| (*backend, (o.model.clone(), o.effort)))
                .collect(),
        )
    }

    /// Pin one backend explicitly (an `--model` / `--effort` the caller already resolved).
    pub fn with(
        mut self,
        backend: BackendId,
        model: Option<String>,
        effort: Option<Effort>,
    ) -> Self {
        self.0.insert(backend, (model, effort));
        self
    }

    fn get(&self, backend: BackendId) -> (Option<&str>, Option<Effort>) {
        match self.0.get(&backend) {
            Some((m, e)) => (m.as_deref(), *e),
            None => (None, None),
        }
    }
}

/// The full set of capability profiles, keyed by [`ProfileKey`]. Holds the score matrix the
/// router consults; built immutably (insert returns nothing surprising — callers either
/// construct via `from_profiles` or fold in one profile at a time).
///
/// **Invariant for the scoring APIs.** [`candidates_for`](Self::candidates_for) and
/// [`best_for`](Self::best_for) assume ONE row per backend — pass them a set that has been
/// collapsed with [`resolved`](Self::resolved). Calling them on a raw multi-variant set would
/// enter the same backend more than once and let a backend outvote itself.
#[derive(Debug, Clone, Default)]
pub struct ProfileSet {
    profiles: BTreeMap<ProfileKey, CapabilityProfile>,
}

impl ProfileSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from an iterator of profiles (last one wins per key).
    pub fn from_profiles<I>(profiles: I) -> Self
    where
        I: IntoIterator<Item = CapabilityProfile>,
    {
        let profiles = profiles.into_iter().map(|p| (p.key(), p)).collect();
        Self { profiles }
    }

    /// Insert/replace the profile for one `(backend, model, effort)` row.
    pub fn insert(&mut self, profile: CapabilityProfile) {
        self.profiles.insert(profile.key(), profile);
    }

    /// The row for `backend` at `(model, effort)`: the exact variant if it has been measured,
    /// else the unpinned row.
    ///
    /// There is deliberately NO partial match — a row measured at `high` is never used to answer
    /// for `low` just because the model happens to agree. Attributing one rung's score to another
    /// is exactly the confusion this keying exists to remove; falling back to the unpinned row
    /// (usually a seeded prior) is the honest answer instead.
    pub fn resolve(
        &self,
        backend: BackendId,
        model: Option<&str>,
        effort: Option<Effort>,
    ) -> Option<&CapabilityProfile> {
        let exact = ProfileKey::new(backend, model.map(str::to_string), effort);
        self.profiles
            .get(&exact)
            .or_else(|| self.profiles.get(&ProfileKey::unpinned(backend)))
    }

    /// Collapse to one row per backend — the variant each backend would actually run under
    /// `pins` — so the scoring APIs see a single candidate per backend. The returned set is
    /// re-keyed as unpinned rows; its `model`/`effort` fields still name what was measured.
    pub fn resolved(&self, pins: &Pins) -> ProfileSet {
        let mut out = BTreeMap::new();
        for backend in self.backends() {
            let (model, effort) = pins.get(backend);
            if let Some(profile) = self.resolve(backend, model, effort) {
                out.insert(ProfileKey::unpinned(backend), profile.clone());
            }
        }
        ProfileSet { profiles: out }
    }

    /// Every backend that has at least one row, in key order.
    pub fn backends(&self) -> Vec<BackendId> {
        let mut seen: Vec<BackendId> = self.profiles.keys().map(|k| k.backend).collect();
        seen.dedup();
        seen
    }

    /// The unpinned row for `backend`. Prefer [`resolve`](Self::resolve) when a dispatch's
    /// model/effort are known.
    pub fn get(&self, backend: BackendId) -> Option<&CapabilityProfile> {
        self.profiles.get(&ProfileKey::unpinned(backend))
    }

    pub fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }

    pub fn len(&self) -> usize {
        self.profiles.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&ProfileKey, &CapabilityProfile)> {
        self.profiles.iter()
    }

    /// Every available backend's score for `category`, in backend order. The router's cost
    /// tiebreak needs the full candidate set, not just the argmax.
    pub fn candidates_for(
        &self,
        category: TaskCategory,
        available: &HashSet<BackendId>,
    ) -> Vec<(BackendId, Score)> {
        self.profiles
            .values()
            .filter(|profile| available.contains(&profile.backend))
            .filter_map(|profile| {
                profile
                    .score(category)
                    .map(|score| (profile.backend, score))
            })
            .collect()
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
            .values()
            .filter(|profile| available.contains(&profile.backend))
            .filter_map(|profile| {
                profile
                    .score(category)
                    .map(|score| (profile.backend, score))
            })
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
            profile_with(BackendId::Opencode, TaskCategory::Coding, 80),
        ]);

        let (backend, score) = set
            .best_for(
                TaskCategory::Coding,
                &available(&[BackendId::Claude, BackendId::Codex, BackendId::Opencode]),
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

    fn variant_with(
        backend: BackendId,
        model: &str,
        effort: Effort,
        category: TaskCategory,
        value: u8,
    ) -> CapabilityProfile {
        let mut p = CapabilityProfile::for_variant(backend, Some(model.into()), Some(effort));
        p.scores.insert(category, Score::seeded(value));
        p
    }

    #[test]
    fn variants_of_one_backend_coexist_instead_of_overwriting() {
        let set = ProfileSet::from_profiles([
            profile_with(BackendId::Codex, TaskCategory::Coding, 60),
            variant_with(
                BackendId::Codex,
                "gpt-5.4-codex",
                Effort::Low,
                TaskCategory::Coding,
                55,
            ),
            variant_with(
                BackendId::Codex,
                "gpt-5.4-codex",
                Effort::XHigh,
                TaskCategory::Coding,
                92,
            ),
        ]);
        assert_eq!(set.len(), 3, "one row per (backend, model, effort)");
        let at = |e| {
            set.resolve(BackendId::Codex, Some("gpt-5.4-codex"), Some(e))
                .unwrap()
                .score(TaskCategory::Coding)
                .unwrap()
                .value
        };
        assert_eq!(at(Effort::Low), 55);
        assert_eq!(at(Effort::XHigh), 92);
    }

    #[test]
    fn an_unmeasured_variant_falls_back_to_the_unpinned_row_not_a_sibling_rung() {
        let set = ProfileSet::from_profiles([
            profile_with(BackendId::Codex, TaskCategory::Coding, 60),
            variant_with(
                BackendId::Codex,
                "gpt-5.4-codex",
                Effort::XHigh,
                TaskCategory::Coding,
                92,
            ),
        ]);
        // `medium` was never measured. Borrowing the xhigh number for it would be exactly the
        // conflation this keying exists to prevent, so the unpinned prior answers instead.
        let medium = set
            .resolve(
                BackendId::Codex,
                Some("gpt-5.4-codex"),
                Some(Effort::Medium),
            )
            .unwrap();
        assert_eq!(medium.score(TaskCategory::Coding).unwrap().value, 60);
        // Same for a different model at the measured rung.
        let other_model = set
            .resolve(BackendId::Codex, Some("gpt-5-codex"), Some(Effort::XHigh))
            .unwrap();
        assert_eq!(other_model.score(TaskCategory::Coding).unwrap().value, 60);
    }

    #[test]
    fn resolved_collapses_to_the_variant_each_backend_would_actually_run() {
        let set = ProfileSet::from_profiles([
            profile_with(BackendId::Codex, TaskCategory::Coding, 60),
            variant_with(
                BackendId::Codex,
                "gpt-5.4-codex",
                Effort::XHigh,
                TaskCategory::Coding,
                92,
            ),
            profile_with(BackendId::Claude, TaskCategory::Coding, 70),
        ]);
        let pins = Pins::default().with(
            BackendId::Codex,
            Some("gpt-5.4-codex".into()),
            Some(Effort::XHigh),
        );
        let view = set.resolved(&pins);

        // One row per backend, so a backend cannot outvote itself in the scoring APIs.
        assert_eq!(view.len(), 2);
        let candidates = view.candidates_for(TaskCategory::Coding, &available(&[BackendId::Codex]));
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].1.value, 92);

        // With no pins, the same set routes on the unpinned rows instead.
        let bare = set.resolved(&Pins::default());
        assert_eq!(
            bare.candidates_for(TaskCategory::Coding, &available(&[BackendId::Codex]))[0]
                .1
                .value,
            60
        );
    }
}
