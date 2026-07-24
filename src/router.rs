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
    /// observability.
    Profile {
        category: TaskCategory,
        score: u8,
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
            RouteReason::Profile { .. } => "profile",
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
            RouteReason::Profile { category, score } => (Some(category.as_str()), Some(score)),
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
            profiles,
            review_keywords_lower,
        }
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
            // Profile-driven diagnostic routing (design §1.6): diagnose the task, and when the
            // verdict is confident enough, send it to the highest-scoring available backend for
            // that category. A shaky diagnosis (low confidence) or a category no available
            // backend has scored falls through to the legacy long-context / keyword heuristics
            // and ultimately `default` — we never let an uncertain guess steer work to an odd
            // backend.
            let diagnosis = diagnose::diagnose(task);
            diagnose_confidence = Some(diagnosis.confidence);
            if diagnosis.confidence >= LLM_ASSIST_CONFIDENCE_THRESHOLD
                && let Some((backend, score)) =
                    self.profiles.best_for(diagnosis.primary, &self.available)
            {
                return RouteDecision {
                    backend,
                    reason: RouteReason::Profile {
                        category: diagnosis.primary,
                        score: score.value,
                    },
                    diagnose_confidence,
                };
            }

            let auto = &self.config.auto_route;
            if self.available.contains(&auto.long_context_backend)
                && estimate_tokens(task) > auto.long_context_threshold
            {
                return RouteDecision {
                    backend: auto.long_context_backend,
                    reason: RouteReason::AutoLongContext,
                    diagnose_confidence,
                };
            }
            if self.available.contains(&auto.review_backend)
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
                .unwrap_or(BackendId::Antigravity)
        };
        RouteDecision {
            backend: final_backend,
            reason: RouteReason::Default,
            diagnose_confidence,
        }
    }
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
        routes.insert(RouteKey::Rescue, BackendId::Gemini);
        routes.insert(RouteKey::Review, BackendId::Claude);
        routes.insert(RouteKey::Explain, BackendId::Gemini);
        routes.insert(RouteKey::Refactor, BackendId::Claude);
        HubConfig {
            default: DefaultSection {
                backend: BackendId::Gemini,
                auto_route: true,
            },
            routes,
            auto_route: AutoRouteSection {
                long_context_threshold: 100,
                long_context_backend: BackendId::Gemini,
                review_keywords: vec!["audit".into(), "review".into()],
                review_backend: BackendId::Claude,
            },
            ensemble: EnsembleSection::default(),
            workflow: WorkflowSection::default(),
            backends: BTreeMap::new(),
        }
    }

    fn available() -> HashSet<BackendId> {
        let mut s = HashSet::new();
        s.insert(BackendId::Gemini);
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
        only_gemini.insert(BackendId::Gemini);
        let r = Router::new(base_config(), only_gemini, ProfileSet::default());
        let d = r.resolve(&RouteRequest {
            tool: RouteKey::Rescue,
            explicit_backend: Some(BackendId::Claude),
            task: Some("x"),
        });
        assert_eq!(d.backend, BackendId::Gemini);
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
        assert_eq!(d.backend, BackendId::Gemini);
        assert_eq!(d.reason, RouteReason::AutoLongContext);
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
        s.insert(BackendId::Gemini);
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
            profile_with(BackendId::Gemini, TaskCategory::Coding, 80),
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
            profile_with(BackendId::Gemini, TaskCategory::Coding, 80),
        ]);
        let r = Router::new(cfg, available(), profiles); // available() has no Codex

        let d = r.resolve(&RouteRequest {
            tool: RouteKey::Rescue,
            explicit_backend: None,
            task: Some("implement a function to parse the duration feature"),
        });

        assert_eq!(d.backend, BackendId::Gemini);
        assert_eq!(
            d.reason,
            RouteReason::Profile {
                category: TaskCategory::Coding,
                score: 80,
            }
        );
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
