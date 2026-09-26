// One-off import of the Studio's localStorage sketches into blueprint files (design
// §4.5). A button, never an automatic migration: only sketches the user actually drew are
// offered (never the seed illustration), and each becomes a linear chain the user then
// edits on the canvas.
//
// Mapping (the same semantic fields deriveSteps writes to [[workflow.steps]]):
// roles[0] → role, backends[0] (else the step's manager backend) → backend,
// persona/behavior → the task; ask → a gate after the step; dynamic → a `manager` node;
// fanout and self-spawning sub-swarms have no blueprint equivalent and are reported.

import { BLUEPRINT_KEY, deriveSteps } from "../studio/blueprint.js";
import { uniqueId } from "./canvas.js";

// Sketch names stored in `storage` (a localStorage-like object): [{key, workflow, steps}].
export function listSketches(storage) {
  const out = [];
  if (!storage) return out;
  const prefix = `${BLUEPRINT_KEY}.`;
  for (let i = 0; i < storage.length; i++) {
    const key = storage.key(i);
    if (!key || !key.startsWith(prefix)) continue;
    try {
      const bp = JSON.parse(storage.getItem(key));
      if (!bp || !Array.isArray(bp.steps) || !bp.goal) continue;
      out.push({ key, workflow: key.slice(prefix.length), steps: bp.steps.length, sketch: bp });
    } catch {
      // not a sketch
    }
  }
  out.sort((a, b) => a.workflow.localeCompare(b.workflow));
  return out;
}

// A blueprint file name from a workflow sketch name ("type.review" → "review").
export function blueprintNameFor(workflow) {
  const base = String(workflow || "")
    .replace(/^type[.-]/, "")
    .toLowerCase()
    .replace(/[^a-z0-9_-]+/g, "-")
    .replace(/^-+|-+$/g, "");
  return (base && /^[a-z0-9]/.test(base) ? base : `sketch-${base || "base"}`).slice(0, 64);
}

function taskFor(step) {
  const lines = ["Goal: {{goal}}"];
  if (step.persona) lines.push("", step.persona);
  if (step.behavior) lines.push("", step.behavior);
  lines.push("", "{{instructions}}");
  return lines.join("\n");
}

// → {doc, warnings[]}. `goalText` of the sketch becomes the goal input's description.
export function sketchToBlueprint(sketch, name) {
  const warnings = [];
  const steps = deriveSteps(sketch);
  const doc = {
    schema: "agentpit.blueprint/1",
    name,
    title: steps.map((s) => s.name).join(" → ") || name,
    inputs: {
      goal: {
        required: true,
        description: String(sketch?.goal?.text || "What to do").replace(/^"|"$/g, ""),
      },
    },
    budget: { max_steps: Math.max(4, steps.length * 3), max_active_secs: 7200, max_parallel: 1 },
    nodes: [],
    edges: [],
    layout: {},
  };
  const byName = new Map((sketch?.steps || []).map((s) => [s.name && s.name.trim(), s]));
  let prev = null;
  const link = (id) => {
    if (prev) doc.edges.push({ from: prev, to: id });
    prev = id;
  };
  let col = 0;
  for (const s of steps) {
    const id = uniqueId(doc, s.name);
    const node = { id, kind: s.dynamic ? "manager" : "agent", title: s.name, task: taskFor(s) };
    if (s.roles[0]) node.role = s.roles[0];
    const backend = s.backends[0] || s.manager_backend;
    if (backend && !node.role) node.backend = backend;
    doc.nodes.push(node);
    doc.layout[id] = { x: 40 + col * 280, y: 80 };
    col += 1;
    link(id);
    if (s.roles.length > 1 || s.backends.length > 1) {
      warnings.push(`${s.name}: only the first role/backend is kept (${[...s.roles, ...s.backends].join(", ")})`);
    }
    if (s.fanout && s.fanout > 1) warnings.push(`${s.name}: fanout ${s.fanout} has no blueprint equivalent and was dropped`);
    if (byName.get(s.name)?.spawns) warnings.push(`${s.name}: the self-spawning sub-swarm was dropped`);
    if (s.ask) {
      const gate = uniqueId(doc, `${id}-ok`);
      doc.nodes.push({ id: gate, kind: "gate", title: `Check ${s.name}`, prompt: `${s.name} is done. Continue?` });
      doc.layout[gate] = { x: 40 + col * 280, y: 80 };
      col += 1;
      link(gate);
    }
  }
  if (!doc.nodes.length) warnings.push("the sketch has no named steps");
  return { doc, warnings };
}
