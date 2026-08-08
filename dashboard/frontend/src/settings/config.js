export const FALLBACK_BACKENDS = ["antigravity", "claude", "codex", "opencode", "prime-agent"];

export const ENSEMBLE_KEYS = [
  "default",
  "review",
  "security_review",
  "adversarial_review",
  "rescue",
  "refactor",
];

const FALLBACK_ENSEMBLE = {
  default: { members: ["antigravity", "claude", "opencode"], aggregator: null },
  review: { members: ["antigravity", "opencode"], aggregator: null },
  security_review: { members: ["claude", "codex"], aggregator: null },
  adversarial_review: { members: ["codex", "antigravity"], aggregator: null },
  rescue: { members: [], aggregator: null },
  refactor: { members: [], aggregator: null },
};

export function draftFromConfig(data = {}) {
  const known = data.known_backends || FALLBACK_BACKENDS;
  const backendMap = new Map((data.backends || []).map((entry) => [entry.id, entry]));
  const sourceEnsemble = data.ensemble || {};
  return {
    config_path: data.config_path || "~/.config/agentpit/config.toml",
    exists: data.exists !== false,
    known_backends: [...known],
    defaults: {
      backend: data.defaults?.backend || "claude",
      auto_route: data.defaults?.auto_route !== false,
    },
    // "" means unpinned: the route falls through to auto-routing and the key stays out
    // of the [routes] table on disk. Never invent a pin the config did not have.
    routes: {
      rescue: data.routes?.rescue || "",
      review: data.routes?.review || "",
      explain: data.routes?.explain || "",
      refactor: data.routes?.refactor || "",
    },
    auto_route: {
      long_context_threshold: data.auto_route?.long_context_threshold ?? 100000,
      long_context_backend: data.auto_route?.long_context_backend || "claude",
      review_keywords_text: (data.auto_route?.review_keywords || ["review", "audit", "critique", "security"]).join(", "),
      review_backend: data.auto_route?.review_backend || "claude",
    },
    ensemble: Object.fromEntries(
      ENSEMBLE_KEYS.map((key) => {
        const entry = sourceEnsemble[key] || FALLBACK_ENSEMBLE[key];
        return [key, { members: [...(entry.members || [])], aggregator: entry.aggregator || "" }];
      })
    ),
    backends: known.map((id) => {
      const entry = backendMap.get(id) || {};
      return {
        id,
        transport: entry.transport || "",
        model: entry.model || "",
      };
    }),
  };
}

export function buildConfigPayload(draft) {
  const keywords = draft.auto_route.review_keywords_text
    .split(",")
    .map((keyword) => keyword.trim())
    .filter(Boolean);
  return {
    defaults: { ...draft.defaults },
    routes: { ...draft.routes },
    auto_route: {
      long_context_threshold: Math.max(0, Number(draft.auto_route.long_context_threshold) || 0),
      long_context_backend: draft.auto_route.long_context_backend,
      review_keywords: [...new Set(keywords)],
      review_backend: draft.auto_route.review_backend,
    },
    ensemble: Object.fromEntries(
      ENSEMBLE_KEYS.map((key) => [
        key,
        {
          members: [...new Set(draft.ensemble[key].members)],
          aggregator: draft.ensemble[key].aggregator || null,
        },
      ])
    ),
    backends: draft.backends.map((entry) => ({
      id: entry.id,
      transport: entry.transport || null,
      model: entry.model.trim() || null,
    })),
  };
}

export function validateConfigDraft(draft) {
  if (!draft) return "設定を読み込めませんでした。";
  const known = new Set(draft.known_backends);
  const backendValues = [
    draft.defaults.backend,
    // An empty route is the unpinned state, not a backend id.
    ...Object.values(draft.routes).filter(Boolean),
    draft.auto_route.long_context_backend,
    draft.auto_route.review_backend,
  ];
  if (backendValues.some((backend) => !known.has(backend))) return "未対応のバックエンドが選択されています。";
  for (const key of ENSEMBLE_KEYS) {
    const members = draft.ensemble[key].members;
    if (members.some((member) => !known.has(member))) return `${key} に未対応のメンバーがあります。`;
    if (new Set(members).size !== members.length) return `${key} に重複したメンバーがあります。`;
  }
  return null;
}
