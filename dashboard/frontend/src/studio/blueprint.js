// Blueprint = the Studio's local canvas sketch (per workflow, in localStorage).
// It is an ILLUSTRATION of a model-driven run — it is never written to config.
// Mirrors the legacy dashboard's keys/shape so the React Studio and the old
// vanilla one read the same saved sketches during the migration.

export const BLUEPRINT_KEY = "agentpit.studio.blueprint.v1";

export function blueprintKey(name) {
  return `${BLUEPRINT_KEY}.${name}`;
}

// The canonical illustration: diagnose → plan → implement → review → integrate,
// with review self-spawning an adversarial sub-swarm.
export function seedBlueprint() {
  return {
    goal: { id: "goal", x: 40, y: 250, w: 210, text: '"Fix the auth flow"' },
    ghost: { id: "ghost", x: 1820, y: 236, w: 156 },
    steps: [
      { id: "s1", index: "01", name: "Diagnose", manager: "antigravity", persona: "Classify the task; pick the best fit from capability profiles.", behavior: "features→category(conf)→backend. LLM assist only on low confidence.", dynamic: false, ask: false, fanout: 1, workers: [{ type: "role", id: "longctx" }], x: 320, y: 200, w: 250 },
      { id: "s2", index: "02", name: "Plan", manager: "claude", persona: "Break the goal into ordered sub-tasks.", behavior: "No static DAG. Improvise on the spot and delegate to the right role.", dynamic: true, ask: false, fanout: 3, workers: [{ type: "role", id: "coder" }], x: 620, y: 200, w: 250 },
      { id: "s3", index: "03", name: "Implement", manager: "claude", persona: "Never stall on reversible work. Dispatch dynamically to the right role.", behavior: "Use rescue / ensemble / workflow as the situation calls for.", dynamic: true, ask: true, fanout: 4, workers: [{ type: "role", id: "coder" }, { type: "role", id: "refactorer" }], x: 920, y: 200, w: 250 },
      { id: "s4", index: "04", name: "Review", manager: "claude", persona: "Check spec violations, boundaries, and security.", behavior: "If unsure, summon a refutation swarm (self-spawns). Over-detection is penalized.", dynamic: true, ask: true, fanout: 3, spawns: true, workers: [{ type: "role", id: "reviewer" }, { type: "role", id: "security" }], x: 1220, y: 200, w: 250 },
      { id: "s5", index: "05", name: "Integrate", manager: "codex", persona: "Integrate findings and finalize the diff.", behavior: "Dedup overlaps. Ask only what only a human can decide.", dynamic: false, ask: true, fanout: 1, workers: [], x: 1520, y: 200, w: 250 },
    ],
    gensteps: [
      { id: "g1", name: "critique", role: "adversary", backend: "codex", x: 1180, y: 520, w: 180 },
      { id: "g2", name: "defense", role: "adversary", backend: "antigravity", x: 1390, y: 520, w: 180 },
      { id: "g3", name: "adjudication", role: "reviewer", backend: "claude", x: 1600, y: 520, w: 180 },
    ],
  };
}

export function loadBlueprint(name) {
  try {
    const raw = localStorage.getItem(blueprintKey(name));
    if (raw) {
      const p = JSON.parse(raw);
      if (p && Array.isArray(p.steps) && p.goal) return p;
    }
  } catch {
    // fall through to seed
  }
  return seedBlueprint();
}

export function saveBlueprint(name, bp) {
  try {
    localStorage.setItem(blueprintKey(name), JSON.stringify(bp));
  } catch {
    // best-effort
  }
}

// The illustrative default flow, used to seed edges when the sketch has none:
// goal → each step in order → ghost, with the self-spawn step feeding its
// gensteps and returning to the next step. Once the user draws their own arrows
// we persist `bp.edges` and use those instead.
export function deriveEdges(bp) {
  const steps = bp.steps || [];
  const gensteps = bp.gensteps || [];
  const edges = [];
  if (steps.length) edges.push({ id: `goal-${steps[0].id}`, source: "goal", target: steps[0].id, kind: "flow" });
  for (let i = 0; i < steps.length - 1; i++) {
    edges.push({ id: `${steps[i].id}-${steps[i + 1].id}`, source: steps[i].id, target: steps[i + 1].id, kind: "handoff" });
  }
  const rev = steps.find((s) => s.spawns);
  if (rev && gensteps.length) {
    edges.push({ id: `${rev.id}-${gensteps[0].id}`, source: rev.id, target: gensteps[0].id, kind: "spawn" });
    for (let i = 0; i < gensteps.length - 1; i++) {
      edges.push({ id: `${gensteps[i].id}-${gensteps[i + 1].id}`, source: gensteps[i].id, target: gensteps[i + 1].id, kind: "subflow" });
    }
    const nxt = steps[steps.indexOf(rev) + 1];
    if (nxt) edges.push({ id: `${gensteps[gensteps.length - 1].id}-${nxt.id}`, source: gensteps[gensteps.length - 1].id, target: nxt.id, kind: "return" });
  }
  if (steps.length) edges.push({ id: `${steps[steps.length - 1].id}-ghost`, source: steps[steps.length - 1].id, target: "ghost", kind: "grow" });
  return edges;
}

export function edgesFor(bp) {
  return Array.isArray(bp.edges) && bp.edges.length ? bp.edges : deriveEdges(bp);
}
