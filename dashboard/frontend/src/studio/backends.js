// Backend visual metadata — mirrors the legacy dashboard's MODEL/CLI_NAME tables
// (public/app.js) so the React Studio renders identical monograms/colors.

export const MODEL = {
  claude: { mono: "C", color: "#d98a6b" },
  codex: { mono: "co", color: "#56b89a" },
  antigravity: { mono: "ag", color: "#c9a227" },
  opencode: { mono: "oc", color: "#7c5cff" },
  goose: { mono: "go", color: "#a7adba" },
  copilot: { mono: "cp", color: "#7d8595" },
};

export const CLI_NAME = {
  claude: "Claude Code",
  codex: "Codex",
  antigravity: "Antigravity",
  opencode: "OpenCode",
};

export function metaFor(id) {
  const m = MODEL[id] || { mono: "•", color: "#7d8595" };
  return { mono: m.mono, color: m.color, label: CLI_NAME[id] || id };
}

// A step worker → chip data. `role` workers resolve against the cast (unknown →
// faint example, matching the vanilla Studio).
export function resolveWorker(w, roles) {
  if (w.type === "role") {
    const r = (roles || []).find((x) => x.name === w.id);
    if (!r) return { color: "#7d8595", mono: "", label: w.id, known: false };
    const primary = r.backends && r.backends[0] ? metaFor(r.backends[0]) : { color: "#7d8595" };
    return { color: primary.color, mono: "", label: r.name || w.id, known: true };
  }
  const m = metaFor(w.id);
  return { color: m.color, mono: m.mono, label: m.label, known: true };
}

export function hexA(hex, a) {
  if (!hex || hex[0] !== "#") return hex;
  const h = hex.slice(1);
  const r = parseInt(h.slice(0, 2), 16);
  const g = parseInt(h.slice(2, 4), 16);
  const b = parseInt(h.slice(4, 6), 16);
  return `rgba(${r}, ${g}, ${b}, ${a})`;
}

// Blueprint step → StudioStepNode `data` (resolves manager + workers to visuals).
export function stepNodeData(st, roles) {
  const m = metaFor(st.manager);
  return {
    index: st.index,
    name: st.name,
    dynamic: st.dynamic,
    managerMono: m.mono,
    managerColor: m.color,
    managerLabel: m.label,
    ask: st.ask,
    persona: st.persona,
    behavior: st.behavior,
    workers: (st.workers || []).map((w) => resolveWorker(w, roles)),
  };
}
