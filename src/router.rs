use std::collections::HashSet;

use crate::config::{HubConfig, RouteKey};
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
    AutoLongContext,
    AutoKeyword,
    Default,
}

impl RouteReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            RouteReason::Explicit => "explicit",
            RouteReason::RouteTable => "route_table",
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
}

pub struct Router {
    config: HubConfig,
    available: HashSet<BackendId>,
    review_keywords_lower: Vec<String>,
}

impl Router {
    pub fn new(config: HubConfig, available: HashSet<BackendId>) -> Self {
        let review_keywords_lower = config
            .auto_route
            .review_keywords
            .iter()
            .map(|k| k.to_ascii_lowercase())
            .collect();
        Self {
            config,
            available,
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
            };
        }

        if let Some(routed) = self.config.routes.get(&request.tool)
            && self.available.contains(routed)
        {
            return RouteDecision {
                backend: *routed,
                reason: RouteReason::RouteTable,
            };
        }

        if self.config.default.auto_route
            && let Some(task) = request.task
        {
            let auto = &self.config.auto_route;
            if self.available.contains(&auto.long_context_backend)
                && estimate_tokens(task) > auto.long_context_threshold
            {
                return RouteDecision {
                    backend: auto.long_context_backend,
                    reason: RouteReason::AutoLongContext,
                };
            }
            if self.available.contains(&auto.review_backend)
                && contains_any_lowercased(task, &self.review_keywords_lower)
            {
                return RouteDecision {
                    backend: auto.review_backend,
                    reason: RouteReason::AutoKeyword,
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
        let r = Router::new(base_config(), available());
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
        let r = Router::new(base_config(), only_gemini);
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
        let r = Router::new(base_config(), available());
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
        let r = Router::new(cfg, available());
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
        let r = Router::new(cfg, available());
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
        let r = Router::new(cfg, available());
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
        let r = Router::new(cfg, available());
        let d = r.resolve(&RouteRequest {
            tool: RouteKey::Rescue,
            explicit_backend: None,
            task: Some("hi"),
        });
        assert_eq!(d.backend, BackendId::Opencode);
        assert_eq!(d.reason, RouteReason::Default);
    }
}
