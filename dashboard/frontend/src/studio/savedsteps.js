// The saved-step library: reusable step templates the user drags onto any
// workflow's canvas. Global (one localStorage key, shared across workflows),
// unlike the per-workflow blueprint. Mirrors the vanilla dashboard.

export const SAVED_STEPS_KEY = "agentpit.studio.savedsteps.v1";

export function loadSavedSteps() {
  try {
    const a = JSON.parse(localStorage.getItem(SAVED_STEPS_KEY) || "[]");
    // Validate element shape too — a corrupt/tampered `[null]` would otherwise
    // crash the palette render on `s.name` (no error boundary in the app).
    return Array.isArray(a) ? a.filter((x) => x && typeof x === "object" && !Array.isArray(x)) : [];
  } catch {
    return [];
  }
}

// Highest N among a blueprint's dropped "st-N" step ids (0 if none). Used to seed
// the drop counter so ids stay unique across remounts/reloads (a fresh useRef(0)
// would otherwise re-mint "st-1" and collide with a persisted step).
export function maxStepSeq(bp) {
  let max = 0;
  for (const s of (bp && bp.steps) || []) {
    const m = /^st-(\d+)$/.exec(s.id || "");
    if (m) max = Math.max(max, parseInt(m[1], 10));
  }
  return max;
}

export function saveSavedSteps(list) {
  try {
    localStorage.setItem(SAVED_STEPS_KEY, JSON.stringify(list));
  } catch {
    // best-effort
  }
}

// Project a blueprint step down to a reusable template: the semantic fields
// only, never geometry (x/y/w) or id/index — those are assigned on drop.
export function stepTemplate(st) {
  return {
    name: st.name || "",
    manager: st.manager || "claude",
    persona: st.persona || "",
    behavior: st.behavior || "",
    dynamic: st.dynamic !== false,
    ask: !!st.ask,
    fanout: st.fanout || 2,
    workers: (st.workers || []).map((w) => ({ type: w.type, id: w.id })),
  };
}

// A fresh blueprint step from a template, placed at `pos`, with a collision-free
// id. `seq` is a monotonic counter the caller owns.
export function stepFromTemplate(tpl, pos, index, seq) {
  const t = stepTemplate(tpl);
  return {
    id: "st-" + seq,
    index: index < 10 ? "0" + index : "" + index,
    ...t,
    x: pos ? Math.round(pos.x) : 320,
    y: pos ? Math.round(pos.y) : 200,
    w: 250,
  };
}
