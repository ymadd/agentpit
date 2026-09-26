// Blueprint documents on a React Flow canvas: layout, the run overlay (a LoopView's node
// states on the frozen blueprint), and the design-mode edits. Pure functions, tested with
// node:test. Every edit returns a new document and keeps keys it does not know, so a
// blueprint written by a newer agentpit (or by hand) round-trips through the Studio.

export const NODE_W = 210;
export const NODE_H = 84;
const GAP_X = 70;
const GAP_Y = 36;
const GROUP_PAD_X = 22;
const GROUP_PAD_TOP = 46;
const GROUP_PAD_BOTTOM = 22;

export const NODE_KINDS = ["agent", "check", "gate", "repeat", "manager"];
export const EDGE_ON = ["ok", "fail", "error", "timeout", "exhausted", "not_ok", "always"];

// Which `on` values each source kind may use (design §4.2).
export function edgeOnFor(kind) {
  switch (kind) {
    case "gate":
      return ["ok", "fail", "timeout", "not_ok", "always"];
    case "repeat":
      return ["ok", "exhausted", "not_ok", "always"];
    default:
      return ["ok", "fail", "error", "timeout", "not_ok", "always"];
  }
}

function nodesOf(doc) {
  return Array.isArray(doc?.nodes) ? doc.nodes.filter((n) => n && typeof n.id === "string") : [];
}

function edgesOf(doc) {
  return Array.isArray(doc?.edges) ? doc.edges.filter((e) => e && e.from && e.to) : [];
}

// A node's group: its `parent` when that is a repeat of this document, else top level
// (so a dangling parent — which validation reports — still draws).
export function groupOf(nodes, n) {
  const parentRepeat = (x) => {
    if (!x?.parent) return undefined;
    const p = nodes.find((y) => y.id === x.parent);
    return p && p.kind === "repeat" ? p : undefined;
  };
  const p = parentRepeat(n);
  if (!p) return undefined;
  // A parent cycle (also reported by validation) would never be drawn: flatten it.
  const seen = new Set([n.id]);
  for (let a = p; a; a = parentRepeat(a)) {
    if (seen.has(a.id)) return undefined;
    seen.add(a.id);
  }
  return p.id;
}

// Children of `parent` (undefined = top level), in declaration order.
function childrenOf(nodes, parent) {
  return nodes.filter((n) => groupOf(nodes, n) === parent);
}

// Longest-path columns within one scope, so a chain reads left to right. Cycles (which
// validation rejects) fall back to declaration order.
function columns(scopeNodes, edges) {
  const ids = new Set(scopeNodes.map((n) => n.id));
  const incoming = new Map(scopeNodes.map((n) => [n.id, []]));
  for (const e of edges) if (ids.has(e.from) && ids.has(e.to) && e.from !== e.to) incoming.get(e.to).push(e.from);
  const col = new Map();
  const visiting = new Set();
  const depth = (id) => {
    if (col.has(id)) return col.get(id);
    if (visiting.has(id)) return 0;
    visiting.add(id);
    let d = 0;
    for (const from of incoming.get(id) || []) d = Math.max(d, depth(from) + 1);
    visiting.delete(id);
    col.set(id, d);
    return d;
  };
  for (const n of scopeNodes) depth(n.id);
  return col;
}

// Sizes and positions for every node: from `layout` when present, else a tidy
// left-to-right flow per scope. Children of a repeat are positioned relative to it (as
// React Flow's parentId expects), and a repeat grows to hold its children. Returns
// Map<id, {x, y, w, h, parent}>.
export function layoutOf(doc) {
  const nodes = nodesOf(doc);
  const edges = edgesOf(doc);
  const saved = (doc && typeof doc.layout === "object" && doc.layout) || {};
  const box = new Map();

  const sizeOf = (n) => {
    const s = saved[n.id] || {};
    if (n.kind !== "repeat") return { w: s.w || NODE_W, h: s.h || NODE_H };
    let w = NODE_W + 2 * GROUP_PAD_X;
    let h = NODE_H + GROUP_PAD_TOP + GROUP_PAD_BOTTOM;
    for (const b of box.values()) {
      if (b.parent !== n.id) continue;
      w = Math.max(w, b.x + b.w + GROUP_PAD_X);
      h = Math.max(h, b.y + b.h + GROUP_PAD_BOTTOM);
    }
    return { w: Math.max(w, s.w || 0), h: Math.max(h, s.h || 0) };
  };

  const place = (parent) => {
    const scope = childrenOf(nodes, parent);
    // Children first: a group's size depends on them.
    for (const n of scope) if (n.kind === "repeat") place(n.id);
    const col = columns(scope, edges);
    const x0 = parent ? GROUP_PAD_X : 40;
    const y0 = parent ? GROUP_PAD_TOP : 40;
    const sizes = new Map(scope.map((n) => [n.id, sizeOf(n)]));
    const colW = new Map();
    for (const n of scope) {
      const c = col.get(n.id) || 0;
      colW.set(c, Math.max(colW.get(c) || 0, sizes.get(n.id).w));
    }
    const colX = new Map();
    let acc = x0;
    for (const c of [...colW.keys()].sort((a, b) => a - b)) {
      colX.set(c, acc);
      acc += colW.get(c) + GAP_X;
    }
    const nextY = new Map();
    for (const n of scope) {
      const c = col.get(n.id) || 0;
      const size = sizes.get(n.id);
      const s = saved[n.id];
      const y = nextY.get(c) ?? y0;
      nextY.set(c, y + size.h + GAP_Y);
      const pos =
        s && isFinite(s.x) && isFinite(s.y) ? { x: s.x, y: s.y } : { x: colX.get(c) ?? x0, y };
      box.set(n.id, { ...pos, w: size.w, h: size.h, parent });
    }
  };
  place(undefined);
  return box;
}

// ---------------------------------------------------------------------------------------
// React Flow graph.

function matchesOn(on, outcome) {
  if (!outcome || outcome === "cancelled") return false;
  const want = on || "ok";
  if (want === "always") return true;
  if (want === "not_ok") return outcome !== "ok";
  return want === outcome;
}

// Parents before children: React Flow requires a group node to precede its children.
function parentFirst(nodes) {
  const byId = new Map(nodes.map((n) => [n.id, n]));
  const depth = (n, seen = new Set()) => {
    const g = groupOf(nodes, n);
    if (!g || seen.has(n.id)) return 0;
    seen.add(n.id);
    return 1 + depth(byId.get(g), seen);
  };
  return nodes
    .map((n, i) => ({ n, d: depth(n), i }))
    .sort((a, b) => a.d - b.d || a.i - b.i)
    .map((x) => x.n);
}

// The graph for a document; with `view` (a LoopView), each node carries its run state.
// `selected` = the selected node id.
export function toFlow(doc, view, selected) {
  const nodes = nodesOf(doc);
  const box = layoutOf(doc);
  const state = new Map((view?.nodes || []).map((v) => [v.node, v]));
  const flowNodes = parentFirst(nodes).map((n) => {
    const b = box.get(n.id) || { x: 0, y: 0, w: NODE_W, h: NODE_H };
    const run = state.get(n.id) || null;
    const group = groupOf(nodes, n);
    return {
      id: n.id,
      type: n.kind === "repeat" ? "loopGroup" : "loopNode",
      position: { x: b.x, y: b.y },
      ...(group ? { parentId: group, extent: "parent" } : {}),
      style: { width: b.w, height: b.h },
      selected: n.id === selected,
      data: nodeData(n, run),
    };
  });
  const flowEdges = edgesOf(doc).map((e, i) => {
    const on = e.on || "ok";
    const from = state.get(e.from);
    const fired = !!(from && from.phase === "done" && matchesOn(on, from.outcome));
    return {
      id: `e${i}:${e.from}->${e.to}:${on}`,
      source: e.from,
      target: e.to,
      label: on === "ok" ? undefined : on,
      className: [`lp-edge-${on}`, fired ? "lp-edge-fired" : "", view ? "lp-edge-run" : ""].join(" ").trim(),
      animated: !!(from && from.phase === "running"),
      data: { index: i, on },
    };
  });
  return { nodes: flowNodes, edges: flowEdges };
}

function nodeData(n, run) {
  const data = {
    id: n.id,
    kind: n.kind || "agent",
    title: n.title || n.id,
    role: n.role || null,
    backend: n.backend || null,
    model: n.model || null,
    command: n.command || null,
    prompt: n.prompt || null,
    task: n.task || null,
    maxIterations: n.max_iterations || null,
  };
  if (!run) return data;
  return {
    ...data,
    run: {
      phase: run.phase,
      outcome: run.outcome || null,
      lastOutcome: run.last_outcome || null,
      attempts: run.attempts || 0,
      runs: run.runs || 0,
      iter: run.iter || [],
      stepId: run.step_id || null,
      gateId: run.gate_id || null,
      assignee: run.assignee || null,
      excerpt: run.excerpt || null,
      error: run.error || null,
      startedTs: run.started_ts || null,
      finishedTs: run.finished_ts || null,
      iteration: run.iteration ? { n: run.iteration.n, max: run.iteration.max, open: run.iteration.open } : null,
    },
  };
}

// ---------------------------------------------------------------------------------------
// Design-mode edits. Each returns a new document; unknown keys are kept.

function clone(doc) {
  return JSON.parse(JSON.stringify(doc || {}));
}

export function uniqueId(doc, base) {
  const taken = new Set(nodesOf(doc).map((n) => n.id));
  const stem = (base || "node").toLowerCase().replace(/[^a-z0-9_-]/g, "") || "node";
  const head = /^[a-z]/.test(stem) ? stem : `n${stem}`;
  if (!taken.has(head)) return head.slice(0, 32);
  for (let i = 2; ; i++) {
    const id = `${head.slice(0, 28)}${i}`;
    if (!taken.has(id)) return id;
  }
}

const DEFAULTS = {
  agent: { task: "Goal: {{goal}}\n" },
  check: { command: "", timeout_secs: 600 },
  gate: { prompt: "Continue?" },
  repeat: { max_iterations: 3, feedback: "last" },
  manager: { task: "Goal: {{goal}}\n" },
};

export function addNode(doc, kind, { parent, x, y } = {}) {
  const next = clone(doc);
  if (!Array.isArray(next.nodes)) next.nodes = [];
  const id = uniqueId(next, kind);
  const node = { id, kind, title: kind[0].toUpperCase() + kind.slice(1), ...(DEFAULTS[kind] || {}) };
  if (parent) node.parent = parent;
  next.nodes.push(node);
  if (isFinite(x) && isFinite(y)) {
    next.layout = { ...(next.layout || {}), [id]: { x: Math.round(x), y: Math.round(y) } };
  }
  return { doc: next, id };
}

// Remove a node, the nodes inside it (a repeat's children), their edges and layout.
export function removeNode(doc, id) {
  const next = clone(doc);
  const gone = new Set([id]);
  let grew = true;
  while (grew) {
    grew = false;
    for (const n of nodesOf(next)) {
      if (n.parent && gone.has(n.parent) && !gone.has(n.id)) {
        gone.add(n.id);
        grew = true;
      }
    }
  }
  next.nodes = (next.nodes || []).filter((n) => !gone.has(n?.id));
  next.edges = (next.edges || []).filter((e) => !gone.has(e?.from) && !gone.has(e?.to));
  if (next.layout) for (const g of gone) delete next.layout[g];
  return next;
}

// Merge `patch` into a node; a key set to undefined or "" (except task/prompt/command,
// which may be empty while editing) is removed.
export function updateNode(doc, id, patch) {
  const next = clone(doc);
  const keepEmpty = new Set(["task", "prompt", "command"]);
  next.nodes = (next.nodes || []).map((n) => {
    if (n?.id !== id) return n;
    const out = { ...n };
    for (const [k, v] of Object.entries(patch)) {
      if (v === undefined || v === null || (v === "" && !keepEmpty.has(k))) delete out[k];
      else out[k] = v;
    }
    return out;
  });
  return next;
}

export function connect(doc, from, to, on = "ok") {
  if (!from || !to || from === to) return doc;
  const next = clone(doc);
  if (!Array.isArray(next.edges)) next.edges = [];
  const exists = next.edges.some((e) => e?.from === from && e?.to === to && (e.on || "ok") === on);
  if (exists) return doc;
  next.edges.push(on === "ok" ? { from, to } : { from, to, on });
  return next;
}

export function removeEdge(doc, index) {
  const next = clone(doc);
  if (Array.isArray(next.edges)) next.edges.splice(index, 1);
  return next;
}

export function setEdgeOn(doc, index, on) {
  const next = clone(doc);
  const e = next.edges?.[index];
  if (!e) return doc;
  if (!on || on === "ok") delete e.on;
  else e.on = on;
  return next;
}

// Record where the user put a node (and a group's size), rounded to whole pixels.
export function moveNode(doc, id, { x, y, w, h }) {
  const next = clone(doc);
  const prev = (next.layout && next.layout[id]) || {};
  const entry = { ...prev, x: Math.round(x), y: Math.round(y) };
  if (isFinite(w)) entry.w = Math.round(w);
  if (isFinite(h)) entry.h = Math.round(h);
  next.layout = { ...(next.layout || {}), [id]: entry };
  return next;
}

// A new, runnable-shaped document for "＋ New blueprint".
export function newBlueprint(name) {
  return {
    schema: "agentpit.blueprint/1",
    name,
    title: name,
    inputs: { goal: { required: true, description: "What to do" } },
    budget: { max_steps: 12, max_active_secs: 3600, max_parallel: 1 },
    nodes: [{ id: "work", kind: "agent", title: "Work", task: "Goal: {{goal}}\n" }],
    edges: [],
  };
}

// The inputs a start form asks for: [{name, required, description, default}].
export function inputsOf(doc) {
  const inputs = (doc && typeof doc.inputs === "object" && doc.inputs) || {};
  return Object.entries(inputs).map(([name, spec]) => ({
    name,
    required: !!spec?.required,
    description: spec?.description || "",
    default: spec?.default ?? "",
  }));
}

// Merge top-level document fields; undefined removes a key. Nodes/edges/layout are
// edited through the functions above.
export function updateDoc(doc, patch) {
  const next = clone(doc);
  for (const [k, v] of Object.entries(patch)) {
    if (v === undefined) delete next[k];
    else next[k] = v;
  }
  return next;
}

// Diagnostics by node id (from `nodes[i]` paths or an explicit `node`), for highlighting.
export function diagnosticsByNode(doc, diagnostics) {
  const nodes = nodesOf(doc);
  const out = new Map();
  for (const d of diagnostics || []) {
    let id = d.node || null;
    const m = /^nodes\[(\d+)\]/.exec(d.path || "");
    if (!id && m && Array.isArray(doc?.nodes)) id = doc.nodes[Number(m[1])]?.id || null;
    if (!id || !nodes.some((n) => n.id === id)) id = "";
    if (!out.has(id)) out.set(id, []);
    out.get(id).push(d);
  }
  return out;
}

// The blueprint file a loop was started from, as a store reference ({scope, name,
// project}), or null when it was inline or lives outside the blueprint directories (then
// the design side shows the frozen copy read-only).
export function fileRefOf(source) {
  if (!source || !source.path) return null;
  const m = /^(.*)\/([a-z0-9][a-z0-9_-]{0,63})\.json$/.exec(source.path);
  if (!m) return null;
  const [, dir, name] = m;
  if (source.scope === "user" && /\/agentpit\/blueprints$/.test(dir)) return { scope: "user", name, project: null };
  const proj = /^(.*)\/\.agentpit\/blueprints$/.exec(dir);
  if (proj) return { scope: "project", name, project: proj[1] };
  return null;
}
