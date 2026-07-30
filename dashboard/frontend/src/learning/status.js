// Data layer + pure view maths for the Learning view.
//
// The report itself is produced by the CLI (`agentpit learning --json`) and handed over by
// the `learning_status` Tauri command, so the desktop view and the terminal never disagree.
// In a plain browser (dev / preview / tests) there is no __TAURI__, so we fall back to a
// global mock the harness can set.

export async function fetchStatus() {
  const invoke = window.__TAURI__?.core?.invoke;
  if (invoke) return await invoke("learning_status");
  if (window.__AGENTPIT_MOCK_LEARNING__) return window.__AGENTPIT_MOCK_LEARNING__;
  throw new Error("learning_status is unavailable outside the desktop app");
}

const clamp01 = (t) => Math.max(0, Math.min(1, t));

// Score → fill. One sequential ramp (slate → teal) so the eye reads *level* from colour and
// nothing else; provenance is a separate badge, never a second hue on the same swatch.
// Below 40 everything sits at the dim end: those cells are all equally "not good at this".
export function scoreColor(value) {
  const t = clamp01((value - 40) / 60);
  const h = Math.round(214 + (162 - 214) * t);
  const s = Math.round(14 + (54 - 14) * t);
  const l = Math.round(22 + (40 - 22) * t);
  return `hsl(${h} ${s}% ${l}%)`;
}

// Single-letter provenance badge, matching `agentpit profile show`'s `src` column.
export const SOURCE_BADGE = { seeded: "s", learned: "L", benchmarked: "B" };

export function pct(n, d) {
  if (!d) return 0;
  return Math.round((n / d) * 100);
}

// Every cell that has telemetry behind it, strongest evidence first. This — not the matrix —
// is what "learning in progress" means: labels accrued against the promotion gate.
export function evidenceCells(status) {
  const out = [];
  for (const row of status.rows || []) {
    for (const cell of row.cells || []) {
      if (cell.evidence) out.push({ backend: row.backend, cell, evidence: cell.evidence });
    }
  }
  return out.sort(
    (a, b) => b.evidence.labels - a.evidence.labels || b.evidence.projected - a.evidence.projected
  );
}

// Coverage donut segments, in trust order (benchmarked → learned → seeded). `len`/`offset`
// are stroke-dasharray/-dashoffset values against a circle of circumference `circumference`.
export function donutSegments(coverage, circumference) {
  const total = coverage?.total || 0;
  const order = ["benchmarked", "learned", "seeded"];
  let used = 0;
  return order.map((key) => {
    const value = coverage?.[key] || 0;
    const len = total ? (value / total) * circumference : 0;
    const segment = { key, value, len, offset: used === 0 ? 0 : -used };
    used += len;
    return segment;
  });
}

// Bars for the label timeline. `scale` is shared by every bar so day-to-day height is
// comparable; an all-zero window still returns bars (height 0) rather than collapsing.
export function timelineBars(timeline) {
  const days = timeline || [];
  const max = days.reduce((m, d) => Math.max(m, d.labels), 0);
  return {
    max,
    bars: days.map((d) => ({
      start_ms: d.start_ms,
      labels: d.labels,
      good: d.good,
      bad: d.bad,
      goodRatio: max ? d.good / max : 0,
      badRatio: max ? d.bad / max : 0,
    })),
  };
}

// "7/24" — the bucket's calendar day in the viewer's locale.
export function dayLabel(startMs) {
  const d = new Date(startMs);
  return `${d.getMonth() + 1}/${d.getDate()}`;
}

// Relative age of the newest label in a cell, coarse. Absent when the log carried no
// timestamp (ts 0), which is a real case for pre-telemetry runs.
export function age(ms, now) {
  if (!ms) return null;
  const minutes = Math.floor(Math.max(0, now - ms) / 60000);
  if (minutes < 1) return "just now";
  if (minutes < 60) return `${minutes}m`;
  if (minutes < 1440) return `${Math.floor(minutes / 60)}h`;
  return `${Math.floor(minutes / 1440)}d`;
}
