use std::collections::HashSet;

use crate::config::{HubConfig, RouteKey};
use crate::diagnose::{self, LLM_ASSIST_CONFIDENCE_THRESHOLD};
use crate::profile::{ProfileSet, TaskCategory};
use crate::types::BackendId;

#[derive(Debug, Clone)]
pub struct RouteRequest<'a> {
    pub tool: RouteKey,
    pub explicit_backend: Option<BackendId>,
    pub task: Option<&'a str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteReason {
    Explicit,
    RouteTable,
    /// Diagnosed task category routed to the highest-scoring available backend via the
    /// capability profiles (design §1.6). Carries the category and the winning score for
    /// observability. `cost_tiebreak` marks that a cheaper backend within the quality
    /// margin was picked over the raw argmax.
    Profile {
        category: TaskCategory,
        score: u8,
        cost_tiebreak: bool,
    },
    /// kNN similarity route: the backend that won sufficiently-similar past tasks
    /// (`--features similarity` builds with the embedding model installed).
    Similarity {
        /// Best neighbour cosine similarity, as a 0–100 percentage.
        sim_pct: u8,
        /// Similar samples backing the winner.
        samples: u16,
    },
    AutoLongContext,
    AutoKeyword,
    Default,
}

impl RouteReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            RouteReason::Explicit => "explicit",
            RouteReason::RouteTable => "route_table",
            RouteReason::Profile {
                cost_tiebreak: false,
                ..
            } => "profile",
            RouteReason::Profile {
                cost_tiebreak: true,
                ..
            } => "profile_cost_tiebreak",
            RouteReason::Similarity { .. } => "similarity",
            RouteReason::AutoLongContext => "auto_long_context",
            RouteReason::AutoKeyword => "auto_keyword",
            RouteReason::Default => "default",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RouteDecision {
    pub backend: BackendId,
    pub reason: RouteReason,
    /// Diagnose confidence when a diagnosis ran during this resolve (any auto-route path,
    /// whichever stage ultimately won). `None` when routing never looked at the task.
    pub diagnose_confidence: Option<f32>,
}

impl RouteDecision {
    /// Emit this decision as a `RouteDecided` event on `logger` (and save the task text under
    /// `tasks/<hash>.txt`). One call per run, right after the run starts.
    pub fn log(&self, logger: &crate::events::RunLogger, task: &str) {
        let (category, score) = match self.reason {
            RouteReason::Profile {
                category, score, ..
            } => (Some(category.as_str()), Some(score)),
            _ => (None, None),
        };
        logger.route_decided(
            self.backend,
            self.reason.as_str(),
            category,
            score,
            self.diagnose_confidence,
            task,
        );
    }
}

pub struct Router {
    config: HubConfig,
    available: HashSet<BackendId>,
    /// Backends the AUTO stages skip because their last dispatch failed durably (quota /
    /// tier / auth) within the cooldown — see [`crate::availability::suspended_backends`].
    /// An explicit `--backend` or a `[routes]` pin is a user decision and is still honored;
    /// so is the default fallback (there is nothing better left to pick by then).
    suspended: HashSet<BackendId>,
    profiles: ProfileSet,
    review_keywords_lower: Vec<String>,
}

impl Router {
    pub fn new(config: HubConfig, available: HashSet<BackendId>, profiles: ProfileSet) -> Self {
        let review_keywords_lower = config
            .auto_route
            .review_keywords
            .iter()
            .map(|k| k.to_ascii_lowercase())
            .collect();
        Self {
            config,
            available,
            suspended: HashSet::new(),
            profiles,
            review_keywords_lower,
        }
    }

    pub fn with_suspended(mut self, suspended: HashSet<BackendId>) -> Self {
        self.suspended = suspended;
        self
    }

    /// Available AND not suspended — the bar every auto-route stage applies.
    fn auto_usable(&self, backend: BackendId) -> bool {
        self.available.contains(&backend) && !self.suspended.contains(&backend)
    }

    pub fn resolve(&self, request: &RouteRequest<'_>) -> RouteDecision {
        if let Some(explicit) = request.explicit_backend
            && self.available.contains(&explicit)
        {
            return RouteDecision {
                backend: explicit,
                reason: RouteReason::Explicit,
                diagnose_confidence: None,
            };
        }

        if let Some(routed) = self.config.routes.get(&request.tool)
            && self.available.contains(routed)
        {
            return RouteDecision {
                backend: *routed,
                reason: RouteReason::RouteTable,
                diagnose_confidence: None,
            };
        }

        let mut diagnose_confidence = None;
        if self.config.default.auto_route
            && let Some(task) = request.task
        {
            // Capacity gate, before every capability stage. Capability (what a backend is good
            // at) and capacity (does the task fit its context) are independent axes: a huge
            // task with a clear category signal used to be captured by the confident diagnosis
            // in the profile stage below and sent to the category argmax whether or not it fits
            // there. A task that exceeds the threshold goes to the designated big-window
            // backend; `diagnose_confidence` stays `None` because routing never classified it.
            let auto = &self.config.auto_route;
            if self.auto_usable(auto.long_context_backend)
                && estimate_tokens(task) > auto.long_context_threshold
            {
                return RouteDecision {
                    backend: auto.long_context_backend,
                    reason: RouteReason::AutoLongContext,
                    diagnose_confidence: None,
                };
            }

            // Similarity stage (before any category diagnosis): a backend that won enough
            // sufficiently-similar past tasks takes the dispatch directly. Compiled out of
            // non-`similarity` builds; inside them every miss (no model, no samples, slow
            // load, thin evidence) falls through to the profile stage below.
            let auto_available: HashSet<BackendId> = self
                .available
                .difference(&self.suspended)
                .copied()
                .collect();

            #[cfg(feature = "similarity")]
            if let Some(pick) = crate::similarity::embed::route(
                task,
                &self.config.auto_route.similarity,
                &auto_available,
            ) {
                return RouteDecision {
                    backend: pick.backend,
                    reason: RouteReason::Similarity {
                        sim_pct: (pick.sim.clamp(0.0, 1.0) * 100.0).round() as u8,
                        samples: pick.samples.min(u16::MAX as usize) as u16,
                    },
                    diagnose_confidence: None,
                };
            }

            // Profile-driven diagnostic routing (design §1.6): diagnose the task, and when the
            // verdict is confident enough, send it to the highest-scoring available backend for
            // that category. A shaky diagnosis (low confidence) or a category no available
            // backend has scored falls through to the legacy long-context / keyword heuristics
            // and ultimately `default` — we never let an uncertain guess steer work to an odd
            // backend.
            let diagnosis = diagnose::diagnose(task);
            diagnose_confidence = Some(diagnosis.confidence);
            if diagnosis.confidence >= LLM_ASSIST_CONFIDENCE_THRESHOLD {
                let candidates = self
                    .profiles
                    .candidates_for(diagnosis.primary, &auto_available);
                let margin = self.config.auto_route.quality_margin;
                let cost_of = |b: BackendId| {
                    self.config
                        .backends
                        .get(&b)
                        .and_then(|o| o.cost)
                        .unwrap_or(50)
                };
                if let Some((backend, score, cost_tiebreak)) =
                    pick_with_cost_tiebreak(&candidates, margin, cost_of)
                {
                    return RouteDecision {
                        backend,
                        reason: RouteReason::Profile {
                            category: diagnosis.primary,
                            score: score.value,
                            cost_tiebreak,
                        },
                        diagnose_confidence,
                    };
                }
            }

            if self.auto_usable(auto.review_backend)
                && contains_any_lowercased(task, &self.review_keywords_lower)
            {
                return RouteDecision {
                    backend: auto.review_backend,
                    reason: RouteReason::AutoKeyword,
                    diagnose_confidence,
                };
            }
        }

        let fallback = self.config.default.backend;
        let final_backend = if self.available.contains(&fallback) {
            fallback
        } else {
            self.available
                .iter()
                .next()
                .copied()
                .unwrap_or(BackendId::Claude)
        };
        RouteDecision {
            backend: final_backend,
            reason: RouteReason::Default,
            diagnose_confidence,
        }
    }
}

/// The profile route's winner with the cost tiebreak applied: among candidates whose score
/// is within `margin` of the best, the cheapest backend wins (ties prefer the higher score,
/// then higher confidence/samples, matching `best_for`'s quality ordering). Returns the
/// picked `(backend, score, tiebreak_applied)`; `None` when there are no candidates.
/// `pub(crate)` so `profile replay` can reproduce the deployed profile stage exactly.
pub(crate) fn pick_with_cost_tiebreak(
    candidates: &[(crate::types::BackendId, crate::profile::Score)],
    margin: u8,
    cost_of: impl Fn(crate::types::BackendId) -> u8,
) -> Option<(crate::types::BackendId, crate::profile::Score, bool)> {
    let (best_backend, best_score) = candidates
        .iter()
        .max_by(|(_, a), (_, b)| {
            a.value
                .cmp(&b.value)
                .then(a.confidence.total_cmp(&b.confidence))
                .then(a.samples.cmp(&b.samples))
        })
        .copied()?;

    let (backend, score) = candidates
        .iter()
        .filter(|(_, s)| best_score.value.saturating_sub(s.value) <= margin)
        .min_by(|(a_backend, a), (b_backend, b)| {
            cost_of(*a_backend)
                .cmp(&cost_of(*b_backend))
                .then(b.value.cmp(&a.value))
                .then(b.confidence.total_cmp(&a.confidence))
                .then(b.samples.cmp(&a.samples))
        })
        .copied()?;
    Some((backend, score, backend != best_backend))
}

fn estimate_tokens(text: &str) -> u64 {
    text.len().div_ceil(4) as u64
}

fn contains_any_lowercased(text: &str, lowercased_keywords: &[String]) -> bool {
    let lower = text.to_ascii_lowercase();
    lowercased_keywords.iter().any(|kw| lower.contains(kw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        AutoRouteSection, DefaultSection, EnsembleSection, HubConfig, WorkflowSection,
    };
    use crate::profile::{CapabilityProfile, Score};
    use std::collections::BTreeMap;

    fn base_config() -> HubConfig {
        let mut routes = BTreeMap::new();
        routes.insert(RouteKey::Rescue, BackendId::Opencode);
        routes.insert(RouteKey::Review, BackendId::Claude);
        routes.insert(RouteKey::Explain, BackendId::Opencode);
        routes.insert(RouteKey::Refactor, BackendId::Claude);
        HubConfig {
            default: DefaultSection {
                backend: BackendId::Opencode,
                auto_route: true,
                cascade: false,
            },
            routes,
            auto_route: AutoRouteSection {
                long_context_threshold: 100,
                long_context_backend: BackendId::Opencode,
                review_keywords: vec!["audit".into(), "review".into()],
                review_backend: BackendId::Claude,
                quality_margin: 5,
                similarity: Default::default(),
            },
            ensemble: EnsembleSection::default(),
            workflow: WorkflowSection::default(),
            backends: BTreeMap::new(),
            cascade: Default::default(),
        }
    }

    fn available() -> HashSet<BackendId> {
        let mut s = HashSet::new();
        s.insert(BackendId::Opencode);
        s.insert(BackendId::Claude);
        s.insert(BackendId::Opencode);
        s
    }

    #[test]
    fn honors_explicit_backend_when_registered() {
        let r = Router::new(base_config(), available(), ProfileSet::default());
        let d = r.resolve(&RouteRequest {
            tool: RouteKey::Rescue,
            explicit_backend: Some(BackendId::Claude),
            task: Some("x"),
        });
        assert_eq!(d.backend, BackendId::Claude);
        assert_eq!(d.reason, RouteReason::Explicit);
    }

    #[test]
    fn ignores_explicit_when_unavailable() {
        let mut only_gemini = HashSet::new();
        only_gemini.insert(BackendId::Opencode);
        let r = Router::new(base_config(), only_gemini, ProfileSet::default());
        let d = r.resolve(&RouteRequest {
            tool: RouteKey::Rescue,
            explicit_backend: Some(BackendId::Claude),
            task: Some("x"),
        });
        assert_eq!(d.backend, BackendId::Opencode);
        assert_eq!(d.reason, RouteReason::RouteTable);
    }

    #[test]
    fn uses_route_table_for_tool() {
        let r = Router::new(base_config(), available(), ProfileSet::default());
        let d = r.resolve(&RouteRequest {
            tool: RouteKey::Review,
            explicit_backend: None,
            task: Some("x"),
        });
        assert_eq!(d.backend, BackendId::Claude);
        assert_eq!(d.reason, RouteReason::RouteTable);
    }

    #[test]
    fn auto_routes_long_context() {
        let mut cfg = base_config();
        cfg.routes.clear();
        let r = Router::new(cfg, available(), ProfileSet::default());
        let long = "x".repeat(10_000);
        let d = r.resolve(&RouteRequest {
            tool: RouteKey::Rescue,
            explicit_backend: None,
            task: Some(&long),
        });
        assert_eq!(d.backend, BackendId::Opencode);
        assert_eq!(d.reason, RouteReason::AutoLongContext);
    }

    #[test]
    fn capacity_beats_capability_for_huge_tasks() {
        // 2026-07 eval finding: a huge task WITH a strong category signal used to be captured
        // by the profile stage (the diagnosis is confident, so the long-context check below it
        // never ran) and got sent to the category argmax regardless of whether it fits there.
        // Capability (what a backend is good at) and capacity (does the task fit) are
        // independent axes — the capacity gate must come first.
        let mut cfg = base_config();
        cfg.routes.clear();
        // Claude is the clear Refactor argmax; Opencode is the long-context backend.
        let profiles = ProfileSet::from_profiles([
            profile_with(BackendId::Claude, TaskCategory::Refactor, 90),
            profile_with(BackendId::Opencode, TaskCategory::Refactor, 40),
        ]);
        let r = Router::new(cfg, available(), profiles);

        let huge = format!("refactor this module: {}", "alpha beta gamma ".repeat(100));
        let d = r.resolve(&RouteRequest {
            tool: RouteKey::Rescue,
            explicit_backend: None,
            task: Some(&huge),
        });

        assert_eq!(d.reason, RouteReason::AutoLongContext);
        assert_eq!(d.backend, BackendId::Opencode);

        // The same task under the threshold routes by capability, as before.
        let small = "refactor this module: alpha beta gamma";
        let d = r.resolve(&RouteRequest {
            tool: RouteKey::Rescue,
            explicit_backend: None,
            task: Some(small),
        });
        assert_eq!(d.backend, BackendId::Claude);
        assert!(matches!(d.reason, RouteReason::Profile { .. }));
    }

    #[test]
    fn auto_routes_by_keyword() {
        let mut cfg = base_config();
        cfg.routes.clear();
        let r = Router::new(cfg, available(), ProfileSet::default());
        let d = r.resolve(&RouteRequest {
            tool: RouteKey::Rescue,
            explicit_backend: None,
            task: Some("please audit this function"),
        });
        assert_eq!(d.backend, BackendId::Claude);
        assert_eq!(d.reason, RouteReason::AutoKeyword);
    }

    #[test]
    fn auto_route_disabled_skips_keyword_match() {
        let mut cfg = base_config();
        cfg.routes.clear();
        cfg.default.auto_route = false;
        let r = Router::new(cfg, available(), ProfileSet::default());
        let d = r.resolve(&RouteRequest {
            tool: RouteKey::Rescue,
            explicit_backend: None,
            task: Some("please audit this function"),
        });
        // auto_route is off → falls through to default
        assert_eq!(d.reason, RouteReason::Default);
    }

    #[test]
    fn falls_back_to_default() {
        let mut cfg = base_config();
        cfg.routes.clear();
        cfg.default = DefaultSection {
            backend: BackendId::Opencode,
            auto_route: false,
            cascade: false,
        };
        let r = Router::new(cfg, available(), ProfileSet::default());
        let d = r.resolve(&RouteRequest {
            tool: RouteKey::Rescue,
            explicit_backend: None,
            task: Some("hi"),
        });
        assert_eq!(d.backend, BackendId::Opencode);
        assert_eq!(d.reason, RouteReason::Default);
    }

    fn profile_with(backend: BackendId, category: TaskCategory, value: u8) -> CapabilityProfile {
        let mut p = CapabilityProfile::seeded(backend);
        p.scores.insert(category, Score::seeded(value));
        p
    }

    fn available_with_codex() -> HashSet<BackendId> {
        let mut s = HashSet::new();
        s.insert(BackendId::Opencode);
        s.insert(BackendId::Claude);
        s.insert(BackendId::Codex);
        s
    }

    #[test]
    fn profile_routes_diagnosed_category_to_argmax_backend() {
        // No route-table entry for the tool, auto_route on, and a confidently-Coding task:
        // the profile stage should win and pick the highest-scoring available backend.
        let mut cfg = base_config();
        cfg.routes.clear();
        let profiles = ProfileSet::from_profiles([
            profile_with(BackendId::Claude, TaskCategory::Coding, 70),
            profile_with(BackendId::Codex, TaskCategory::Coding, 90),
            profile_with(BackendId::Opencode, TaskCategory::Coding, 80),
        ]);
        let r = Router::new(cfg, available_with_codex(), profiles);

        let d = r.resolve(&RouteRequest {
            tool: RouteKey::Rescue,
            explicit_backend: None,
            task: Some("implement a function to parse the duration feature"),
        });

        assert_eq!(d.backend, BackendId::Codex);
        assert_eq!(
            d.reason,
            RouteReason::Profile {
                category: TaskCategory::Coding,
                score: 90,
                cost_tiebreak: false,
            }
        );
        assert_eq!(d.reason.as_str(), "profile");
    }

    #[test]
    fn profile_argmax_respects_availability() {
        // Codex is the global best at Coding but is not registered — Gemini wins among the
        // available backends, never an offline one.
        let mut cfg = base_config();
        cfg.routes.clear();
        let profiles = ProfileSet::from_profiles([
            profile_with(BackendId::Claude, TaskCategory::Coding, 70),
            profile_with(BackendId::Codex, TaskCategory::Coding, 90),
            profile_with(BackendId::Opencode, TaskCategory::Coding, 80),
        ]);
        let r = Router::new(cfg, available(), profiles); // available() has no Codex

        let d = r.resolve(&RouteRequest {
            tool: RouteKey::Rescue,
            explicit_backend: None,
            task: Some("implement a function to parse the duration feature"),
        });

        assert_eq!(d.backend, BackendId::Opencode);
        assert_eq!(
            d.reason,
            RouteReason::Profile {
                category: TaskCategory::Coding,
                score: 80,
                cost_tiebreak: false,
            }
        );
    }

    #[test]
    fn cost_tiebreak_picks_cheapest_within_margin_only() {
        use crate::config::BackendOverride;

        let mut cfg = base_config();
        cfg.routes.clear();
        cfg.auto_route.quality_margin = 5;
        // Codex is the argmax (90) but pricey; Gemini scores within the margin (88) and is
        // nearly free; Claude is also cheap but out of margin (70).
        for (backend, cost) in [
            (BackendId::Codex, 80u8),
            (BackendId::Opencode, 5),
            (BackendId::Claude, 5),
        ] {
            cfg.backends.insert(
                backend,
                BackendOverride {
                    cost: Some(cost),
                    ..BackendOverride::default()
                },
            );
        }
        let profiles = ProfileSet::from_profiles([
            profile_with(BackendId::Claude, TaskCategory::Coding, 70),
            profile_with(BackendId::Codex, TaskCategory::Coding, 90),
            profile_with(BackendId::Opencode, TaskCategory::Coding, 88),
        ]);
        let r = Router::new(cfg.clone(), available_with_codex(), profiles.clone());

        let d = r.resolve(&RouteRequest {
            tool: RouteKey::Rescue,
            explicit_backend: None,
            task: Some("implement a function to parse the duration feature"),
        });
        assert_eq!(d.backend, BackendId::Opencode);
        assert_eq!(
            d.reason,
            RouteReason::Profile {
                category: TaskCategory::Coding,
                score: 88,
                cost_tiebreak: true,
            }
        );
        assert_eq!(d.reason.as_str(), "profile_cost_tiebreak");

        // Margin 0: only true score ties are interchangeable — the argmax stays.
        let mut tight = cfg;
        tight.auto_route.quality_margin = 0;
        let r = Router::new(tight, available_with_codex(), profiles);
        let d = r.resolve(&RouteRequest {
            tool: RouteKey::Rescue,
            explicit_backend: None,
            task: Some("implement a function to parse the duration feature"),
        });
        assert_eq!(d.backend, BackendId::Codex);
        assert_eq!(d.reason.as_str(), "profile");
    }

    #[test]
    fn cost_tiebreak_on_equal_scores_prefers_cheaper_backend() {
        use crate::config::BackendOverride;

        let mut cfg = base_config();
        cfg.routes.clear();
        cfg.backends.insert(
            BackendId::Opencode,
            BackendOverride {
                cost: Some(0),
                ..BackendOverride::default()
            },
        );
        // Codex has no configured cost → mid-range 50; equal score → free Gemini wins.
        let profiles = ProfileSet::from_profiles([
            profile_with(BackendId::Codex, TaskCategory::Coding, 90),
            profile_with(BackendId::Opencode, TaskCategory::Coding, 90),
        ]);
        let r = Router::new(cfg, available_with_codex(), profiles);
        let d = r.resolve(&RouteRequest {
            tool: RouteKey::Rescue,
            explicit_backend: None,
            task: Some("implement a function to parse the duration feature"),
        });
        assert_eq!(d.backend, BackendId::Opencode);
    }

    #[test]
    fn suspension_skips_auto_stages_but_never_explicit_or_pins() {
        // The profile argmax (Codex, 90) is suspended after a durable failure: the profile
        // stage must pick the next best available backend instead.
        let mut cfg = base_config();
        cfg.routes.clear();
        let profiles = ProfileSet::from_profiles([
            profile_with(BackendId::Claude, TaskCategory::Coding, 70),
            profile_with(BackendId::Codex, TaskCategory::Coding, 90),
            profile_with(BackendId::Opencode, TaskCategory::Coding, 80),
        ]);
        let suspended: HashSet<BackendId> = [BackendId::Codex].into_iter().collect();
        let r = Router::new(cfg, available_with_codex(), profiles.clone())
            .with_suspended(suspended.clone());

        let task = "implement a function to parse the duration feature";
        let d = r.resolve(&RouteRequest {
            tool: RouteKey::Rescue,
            explicit_backend: None,
            task: Some(task),
        });
        assert_eq!(d.backend, BackendId::Opencode);
        assert!(matches!(d.reason, RouteReason::Profile { score: 80, .. }));

        // An explicit pick is a user decision — suspension never blocks it.
        let d = r.resolve(&RouteRequest {
            tool: RouteKey::Rescue,
            explicit_backend: Some(BackendId::Codex),
            task: Some(task),
        });
        assert_eq!(d.backend, BackendId::Codex);
        assert_eq!(d.reason, RouteReason::Explicit);

        // So is a [routes] pin.
        let mut pinned = base_config();
        pinned.routes.clear();
        pinned.routes.insert(RouteKey::Review, BackendId::Codex);
        let r = Router::new(pinned, available_with_codex(), profiles).with_suspended(suspended);
        let d = r.resolve(&RouteRequest {
            tool: RouteKey::Review,
            explicit_backend: None,
            task: Some(task),
        });
        assert_eq!(d.backend, BackendId::Codex);
        assert_eq!(d.reason, RouteReason::RouteTable);
    }

    #[test]
    fn suspended_review_backend_falls_through_the_keyword_stage() {
        // base_config routes review keywords to Claude; with Claude suspended the keyword
        // stage must not fire, leaving the task to the default backend.
        let mut cfg = base_config();
        cfg.routes.clear();
        let suspended: HashSet<BackendId> = [cfg.auto_route.review_backend].into_iter().collect();
        let r = Router::new(cfg, available(), ProfileSet::default()).with_suspended(suspended);
        let d = r.resolve(&RouteRequest {
            tool: RouteKey::Rescue,
            explicit_backend: None,
            task: Some("please audit this function"),
        });
        assert_ne!(d.reason, RouteReason::AutoKeyword);
        assert_eq!(d.reason, RouteReason::Default);
        assert_eq!(d.backend, BackendId::Opencode);
    }

    #[test]
    fn suspended_long_context_backend_falls_through_the_capacity_gate() {
        // The capacity gate's target is quota-dead: skip the gate rather than dispatch a
        // huge task into a guaranteed failure. With no other signal the task lands on the
        // default backend (which stays reachable — by then there is nothing better left).
        let mut cfg = base_config();
        cfg.routes.clear();
        let suspended: HashSet<BackendId> =
            [cfg.auto_route.long_context_backend].into_iter().collect();
        let r = Router::new(cfg, available(), ProfileSet::default()).with_suspended(suspended);
        let long = "x".repeat(10_000);
        let d = r.resolve(&RouteRequest {
            tool: RouteKey::Rescue,
            explicit_backend: None,
            task: Some(&long),
        });
        assert_ne!(d.reason, RouteReason::AutoLongContext);
        assert_eq!(d.reason, RouteReason::Default);
    }

    #[test]
    fn diagnose_confidence_is_carried_only_when_a_diagnosis_ran() {
        // Explicit route: routing never looked at the task → None.
        let r = Router::new(base_config(), available(), ProfileSet::default());
        let d = r.resolve(&RouteRequest {
            tool: RouteKey::Rescue,
            explicit_backend: Some(BackendId::Claude),
            task: Some("x"),
        });
        assert_eq!(d.diagnose_confidence, None);

        // Auto-route path (profile or fall-through): the diagnosis ran → Some.
        let mut cfg = base_config();
        cfg.routes.clear();
        let r = Router::new(cfg, available(), ProfileSet::default());
        let d = r.resolve(&RouteRequest {
            tool: RouteKey::Rescue,
            explicit_backend: None,
            task: Some("implement a function to parse the duration feature"),
        });
        assert!(d.diagnose_confidence.is_some());
    }

    #[test]
    fn low_confidence_diagnosis_does_not_take_profile_path() {
        // A signal-free task diagnoses to a low-confidence category. Even though the profiles
        // do score that category, the confidence gate must keep us off the profile path so a
        // misclassification can't steer work to an odd backend — we fall through to default.
        let mut cfg = base_config();
        cfg.routes.clear();
        cfg.default.backend = BackendId::Opencode;
        // A profile that would have sent Coding to Codex if the gate let it through.
        let profiles =
            ProfileSet::from_profiles([profile_with(BackendId::Codex, TaskCategory::Coding, 99)]);
        let mut avail = available();
        avail.insert(BackendId::Codex);
        let r = Router::new(cfg, avail, profiles);

        let d = r.resolve(&RouteRequest {
            tool: RouteKey::Rescue,
            explicit_backend: None,
            task: Some("alpha beta gamma"),
        });

        // Profile path skipped (low confidence), no long-context, no keyword → default.
        assert_eq!(d.reason, RouteReason::Default);
        assert_eq!(d.backend, BackendId::Opencode);
    }
}
