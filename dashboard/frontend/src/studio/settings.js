// Config-editing layer for the Studio: turn `settings_get` data into an editable
// draft, validate it, and assemble the exact `settings_save` payload (mirrors
// the legacy dashboard's saveSettings + settings.rs contract).

export const ROLE_NAME_RE = /^[a-z0-9][a-z0-9_-]*$/;
export const DEFAULT_MAX_DEPTH = 3;
export const DEFAULT_MAX_CALLS = 8;
export const DEFAULT_BACKENDS = ["claude", "codex", "gemini", "antigravity", "opencode"];

let roleKeySeq = 0;

export function draftFromSettings(data) {
  const wf = (data && data.workflow) || {};
  return {
    known_backends: (data && data.known_backends) || DEFAULT_BACKENDS,
    workflow: {
      manager_backend: wf.manager_backend || "",
      default_agents: wf.default_agents || [],
      max_depth: wf.max_depth ?? DEFAULT_MAX_DEPTH,
      max_calls_per_manager: wf.max_calls_per_manager ?? DEFAULT_MAX_CALLS,
      use_mcp: !!wf.use_mcp,
      enable_ask_human: !!wf.enable_ask_human,
    },
    roles: ((data && data.roles) || []).map((r) => ({
      _key: ++roleKeySeq,
      name: r.name || "",
      backends: [...(r.backends || [])],
      prompt: r.prompt || "",
      model: r.model || "",
    })),
    // Not edited in this slice — round-tripped unchanged so Save never drops them.
    types: ((data && data.types) || []).map((t) => ({ ...t })),
  };
}

export function newRole() {
  return { _key: ++roleKeySeq, name: "", backends: [], prompt: "", model: "" };
}

export function roleNameError(name, roles, selfKey) {
  if (!name) return "Enter a name";
  if (!ROLE_NAME_RE.test(name)) return "Only lowercase letters, digits, - and _ (must start alphanumeric)";
  if (roles.some((r) => r._key !== selfKey && r.name === name)) return "This name is already in use";
  return null;
}

export function validate(draft) {
  const errors = {};
  for (const r of draft.roles) {
    const m = roleNameError(r.name, draft.roles, r._key);
    if (m) errors[r._key] = m;
  }
  return { ok: Object.keys(errors).length === 0, errors };
}

export function buildPayload(draft) {
  return {
    workflow: {
      manager_backend: draft.workflow.manager_backend || null,
      default_agents: draft.workflow.default_agents || [],
      max_depth: draft.workflow.max_depth ?? DEFAULT_MAX_DEPTH,
      max_calls_per_manager: draft.workflow.max_calls_per_manager ?? DEFAULT_MAX_CALLS,
      use_mcp: !!draft.workflow.use_mcp,
      enable_ask_human: !!draft.workflow.enable_ask_human,
    },
    roles: draft.roles.map((r) => ({
      name: r.name,
      backends: r.backends,
      prompt: r.prompt,
      model: r.model || null,
    })),
    types: (draft.types || []).map((t) => ({
      name: t.name,
      title: t.title || null,
      description: t.description || null,
      prompt: t.prompt || null,
      roles: t.roles || [],
      manager_backend: t.manager_backend || null,
      max_depth: t.max_depth ?? null,
      max_calls_per_manager: t.max_calls_per_manager ?? null,
      use_mcp: t.use_mcp ?? null,
      enable_ask_human: t.enable_ask_human ?? null,
    })),
  };
}
