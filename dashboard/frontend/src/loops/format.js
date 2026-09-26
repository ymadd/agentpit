// Pure presentation of loop rows, gates and asks (no React, no Tauri) — tested with
// node:test. The bridge sends LoopSummary rows exactly as `agentpit loop ls --json`
// prints them, so every label here is derived, never stored.

// The last 8 hex digits tell loops apart on a board (the CLI's `short`).
export function shortId(loopId) {
  return (loopId || "").slice(-8);
}

// One word for where a loop is. `waiting` = running, but only a person can move it on;
// `parked` = no runner is alive (it wakes when answered, or on its deadline).
export function stateOf(row) {
  const s = row?.summary || {};
  if (s.status === "succeeded" || s.status === "failed" || s.status === "cancelled") return s.status;
  if (s.status === "created") return "created";
  if (s.stop || s.status === "stopping") return "stopping";
  if (s.pause || s.status === "paused") return "paused";
  if (s.waiting) return row.runner === "live" ? "waiting" : "parked";
  if (s.status === "running" && row.runner !== "live") return "stalled";
  return "running";
}

export const TERMINAL = new Set(["succeeded", "failed", "cancelled"]);

// "fix 2/4 · implement (claude)" — the stage and who has it (design §12: 段階・担当).
export function stageText(summary) {
  if (!summary) return "";
  const parts = (summary.active || []).map((a) => {
    const iter = a.iter && a.iter.length ? `#${a.iter.join(".")}` : "";
    // A gate step is someone waiting on a person, not an agent at work.
    if (a.kind === "gate") return `? ${a.node}${iter}`;
    const who = a.assignee ? a.assignee.role || a.assignee.backend : "";
    return who ? `${a.node}${iter} (${who})` : `${a.node}${iter}`;
  });
  const loops = (summary.iterations || [])
    .filter((i) => i.open)
    .map((i) => `${i.repeat} ${i.n}/${i.max}`);
  const out = [...loops, ...parts];
  if (summary.finish) {
    out.push(summary.finish.node ? `${summary.finish.reason} @ ${summary.finish.node}` : summary.finish.reason);
  }
  return out.join(" · ");
}

// Everyone working on the loop right now: [{node, who, kind}].
export function assignees(summary) {
  return (summary?.active || [])
    .filter((a) => a.assignee)
    .map((a) => ({
      node: a.node,
      kind: a.kind,
      who: a.assignee.role ? `${a.assignee.role} · ${a.assignee.backend}` : a.assignee.backend,
      model: a.assignee.model || null,
    }));
}

export function duration(ms) {
  if (ms == null || !isFinite(ms) || ms < 0) return "";
  const s = Math.floor(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m${String(s % 60).padStart(2, "0")}s`;
  const h = Math.floor(m / 60);
  return `${h}h${String(m % 60).padStart(2, "0")}m`;
}

// "3m ago" style age of a timestamp (ms), for the board's updated column.
export function ago(ts, now = Date.now()) {
  if (!ts) return "";
  const d = Math.max(0, now - ts);
  if (d < 45_000) return "now";
  return duration(d);
}

// Budget use as fractions (0..1), for the row's bars.
export function budgetUse(summary) {
  const u = summary?.usage || {};
  const b = summary?.budget || {};
  const frac = (a, m) => (m > 0 ? Math.min(1, (a || 0) / m) : 0);
  return {
    steps: frac(u.steps, b.max_steps),
    time: frac((u.active_ms || 0) / 1000, b.max_active_secs),
    stepsText: `${u.steps || 0}/${b.max_steps || 0}`,
    timeText: `${duration(u.active_ms || 0)} / ${duration((b.max_active_secs || 0) * 1000)}`,
  };
}

// Board sections: loops that need a person first, then running ones, then the rest.
const ORDER = { waiting: 0, parked: 0, stalled: 1, running: 2, stopping: 2, paused: 3, created: 4 };

export function boardSections(rows) {
  const active = [];
  const finished = [];
  for (const row of rows || []) {
    const state = stateOf(row);
    (TERMINAL.has(state) ? finished : active).push({ row, state });
  }
  active.sort(
    (a, b) =>
      (ORDER[a.state] ?? 9) - (ORDER[b.state] ?? 9) ||
      (b.row.summary.updated_ts || 0) - (a.row.summary.updated_ts || 0)
  );
  finished.sort((a, b) => (b.row.summary.updated_ts || 0) - (a.row.summary.updated_ts || 0));
  return { active, finished };
}

// A gate's choices; an approval gate without options is approve/reject (the blueprint
// default), so the inbox always has buttons.
export function gateOptions(gate) {
  const opts = gate?.options || [];
  if (opts.length) return opts.map((o) => ({ id: o.id, label: o.label || o.id, outcome: o.outcome || null }));
  return [
    { id: "approve", label: "Approve", outcome: "ok" },
    { id: "reject", label: "Reject", outcome: "fail" },
  ];
}

// The inbox: loop gates and the existing asks as one list. Asks keep their own shape
// (answered through answer_ask); gates go through gate_resolve. Soonest deadline first,
// then oldest.
export function inboxItems(inbox, asks, now = Date.now()) {
  const items = [];
  for (const g of inbox?.gates || []) {
    items.push({
      type: "gate",
      key: `gate:${g.loop_id}:${g.gate_id}`,
      loopId: g.loop_id,
      title: g.loop_title,
      blueprint: g.blueprint,
      gateId: g.gate_id,
      kind: g.kind,
      node: g.node || null,
      prompt: g.prompt,
      options: gateOptions(g),
      parked: !!g.parked,
      deadline: g.deadline_ms || null,
      order: g.deadline_ms || Number.MAX_SAFE_INTEGER,
    });
  }
  for (const a of asks || []) {
    const deadline = a.timeoutSecs ? a.ts + a.timeoutSecs * 1000 : null;
    items.push({
      type: "ask",
      key: `ask:${a.askId}`,
      askId: a.askId,
      runId: a.runId,
      title: a.kind === "blocking" ? "Blocking question" : "Review",
      kind: a.kind,
      prompt: a.prompt,
      options: (a.options && a.options.length ? a.options : ["yes", "no"]).map((o) => ({ id: o, label: o })),
      deadline,
      ts: a.ts,
      order: deadline || Number.MAX_SAFE_INTEGER,
    });
  }
  items.sort((a, b) => a.order - b.order || (a.ts || 0) - (b.ts || 0) || a.key.localeCompare(b.key));
  for (const it of items) it.overdue = it.deadline != null && it.deadline < now;
  return items;
}

// "answered approve by agentpit-dashboard/0.3.0 (human)" for the second window.
export function answeredText(a) {
  if (!a) return "";
  const who = a.by?.client || a.by?.kind || "someone";
  const how = a.option_label || a.option;
  return a.comment ? `${how} — ${who}: ${a.comment}` : `${how} — ${who}`;
}

// The launcher's counts: loops running, and decisions waiting (gates + asks).
export function launcherCounts(rows, inbox, asks) {
  let running = 0;
  for (const row of rows || []) {
    const st = stateOf(row);
    if (st === "running" || st === "stopping") running += 1;
  }
  const gates = (inbox?.gates?.length || 0) + (inbox?.hidden || 0);
  return { running, decisions: gates + (asks?.length || 0) };
}
