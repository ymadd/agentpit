// Config-editing layer for the Studio: turn `settings_get` data into an editable
// draft, validate it, and assemble the exact `settings_save` payload (mirrors
// the legacy dashboard's saveSettings + settings.rs contract).

export const ROLE_NAME_RE = /^[a-z0-9][a-z0-9_-]*$/;
export const DEFAULT_MAX_DEPTH = 3;
export const DEFAULT_MAX_CALLS = 8;
export const DEFAULT_BACKENDS = ["claude", "codex", "antigravity", "opencode"];
export const DEFAULT_RESERVED_TYPE_NAMES = ["new", "list", "describe"];

let roleKeySeq = 0;
let typeKeySeq = 0;

// A settings-draft type: knob overrides are null = inherit base (the vanilla
// null-preserving convention), and _key/isNew are client-only (stripped on save).
function typeFromConfig(t) {
  return {
    _key: ++typeKeySeq,
    name: t.name || "",
    title: t.title || "",
    description: t.description || "",
    prompt: t.prompt || "",
    roles: [...(t.roles || [])],
    manager_backend: t.manager_backend || "",
    max_depth: t.max_depth ?? null,
    max_calls_per_manager: t.max_calls_per_manager ?? null,
    use_mcp: t.use_mcp == null ? null : !!t.use_mcp,
    enable_ask_human: t.enable_ask_human == null ? null : !!t.enable_ask_human,
    // Phase 4: soft flow hint (derived from the drawn edges; recomputed on Save).
    flow: t.flow || "",
    // The structured plan (`[[workflow.types.*.steps]]`), likewise recomputed on Save.
    steps: [...(t.steps || [])],
    isNew: false,
  };
}

export function draftFromSettings(data) {
  const wf = (data && data.workflow) || {};
  const knownBackends = (data && data.known_backends) || DEFAULT_BACKENDS;
  const configuredModels = (data && data.backend_models) || {};
  return {
    known_backends: knownBackends,
    backend_models: Object.fromEntries(knownBackends.map((backend) => [backend, configuredModels[backend] || ""])),
    reserved_type_names: (data && data.reserved_type_names) || DEFAULT_RESERVED_TYPE_NAMES,
    workflow: {
      manager_backend: wf.manager_backend || "",
      default_agents: wf.default_agents || [],
      max_depth: wf.max_depth ?? DEFAULT_MAX_DEPTH,
      max_calls_per_manager: wf.max_calls_per_manager ?? DEFAULT_MAX_CALLS,
      use_mcp: !!wf.use_mcp,
      enable_ask_human: !!wf.enable_ask_human,
      // Soft flow hint for the BASE canvas, recomputed from the sketch on Save.
      flow: wf.flow || "",
      steps: [...(wf.steps || [])],
    },
    roles: ((data && data.roles) || []).map((r) => ({
      _key: ++roleKeySeq,
      name: r.name || "",
      backends: [...(r.backends || [])],
      prompt: r.prompt || "",
      model: r.model || "",
    })),
    types: ((data && data.types) || []).map(typeFromConfig),
  };
}

export function newRole() {
  return { _key: ++roleKeySeq, name: "", backends: [], prompt: "", model: "" };
}

export function newType() {
  return {
    _key: ++typeKeySeq,
    name: "",
    title: "",
    description: "",
    prompt: "",
    roles: [],
    manager_backend: "",
    max_depth: null,
    max_calls_per_manager: null,
    use_mcp: null,
    enable_ask_human: null,
    isNew: true,
  };
}

// Mirror of settings.rs valid_role_name + reserved-type rules (client hint; the
// backend is the gate). Order matches the vanilla: empty → reserved → regex → dup.
export function typeNameError(name, types, selfKey, reserved) {
  if (!name) return "Enter a name";
  if ((reserved || DEFAULT_RESERVED_TYPE_NAMES).includes(name)) return "This name is reserved (an agentpit workflow subcommand)";
  if (!ROLE_NAME_RE.test(name)) return "Only lowercase letters, digits, - and _ (must start alphanumeric)";
  if (types.some((t) => t._key !== selfKey && t.name === name)) return "This name is already in use";
  return null;
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
  for (const t of draft.types || []) {
    const m = typeNameError(t.name, draft.types, t._key, draft.reserved_type_names);
    if (m) errors["t" + t._key] = m;
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
      flow: draft.workflow.flow && draft.workflow.flow.trim() ? draft.workflow.flow : null,
      steps: draft.workflow.steps || [],
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
      flow: t.flow && t.flow.trim() ? t.flow : null,
      steps: t.steps || [],
    })),
  };
}
