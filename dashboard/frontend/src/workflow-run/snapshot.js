// Data layer for the live "Workflow Run" stage view.
//
// Source of truth is the Tauri backend's `get_snapshot` command (same one the
// legacy swarm view polls). In a plain browser (dev / preview / tests) there is
// no __TAURI__, so we fall back to a global mock the harness can set.
//
// A workflow's "stages" are MODEL-DRIVEN: there is no static stage list in
// config. So we derive them from what the manager actually dispatched — every
// run whose `parent_run_id` chains back to the workflow root is a stage, in
// dispatch order. This mirrors exactly the event-tree data the dashboard emits.

export async function getSnapshot() {
  const invoke = window.__TAURI__?.core?.invoke;
  if (invoke) {
    try {
      return await invoke("get_snapshot");
    } catch {
      // fall through to mock/empty
    }
  }
  return window.__AGENTPIT_MOCK_SNAPSHOT__ || { live: [], recent: [] };
}

// running | done | failed — collapse the run/member status vocabulary into the
// three states the stage graph cares about.
export function runStatus(run) {
  if (!run.finished) return "running";
  const s = run.status;
  if (s === "error" || s === "interrupted") return "failed";
  if ((run.members || []).some((m) => m.status === "error")) return "failed";
  return "done";
}

// Pick the workflow to visualize: a live (still-running) workflow if any,
// otherwise the most recently started workflow so you can review the last run.
export function pickWorkflow(snapshot) {
  const all = [...(snapshot.live || []), ...(snapshot.recent || [])];
  const workflows = all.filter((r) => r.kind === "workflow");
  if (!workflows.length) return null;
  const liveWf = (snapshot.live || []).filter((r) => r.kind === "workflow");
  const pool = liveWf.length ? liveWf : workflows;
  return pool.reduce((a, b) => (b.started_ts > a.started_ts ? b : a));
}

function backendLabel(run) {
  const names = (run.members || []).map((m) => m.backend).filter(Boolean);
  const uniq = [...new Set(names)];
  return uniq.join(", ");
}

// Build React Flow {nodes, edges} for the workflow rooted at `root`, laying the
// dispatch tree out left→right by depth, siblings stacked by start order.
export function buildStageGraph(snapshot, root) {
  const all = [...(snapshot.live || []), ...(snapshot.recent || [])];
  const byId = new Map(all.map((r) => [r.run_id, r]));
  // de-dup: a run can appear in both live and recent across a transition
  const kids = new Map();
  for (const r of all) {
    const p = r.parent_run_id;
    if (p && byId.has(p)) {
      if (!kids.has(p)) kids.set(p, []);
      if (!kids.get(p).some((x) => x.run_id === r.run_id)) kids.get(p).push(r);
    }
  }
  for (const list of kids.values()) list.sort((a, b) => a.started_ts - b.started_ts);

  const nodes = [];
  const edges = [];
  const seen = new Set();
  const levelCount = [];
  const COL = 260;
  const ROW = 132;

  const walk = (run, depth) => {
    if (seen.has(run.run_id)) return;
    seen.add(run.run_id);
    const row = levelCount[depth] || 0;
    levelCount[depth] = row + 1;
    const isRoot = run.run_id === root.run_id;
    const status = runStatus(run);
    nodes.push({
      id: run.run_id,
      type: "stage",
      position: { x: 40 + depth * COL, y: 30 + row * ROW },
      data: {
        title: isRoot ? "WORKFLOW" : run.role || run.kind,
        subtitle: isRoot ? backendLabel(run) || "manager" : backendLabel(run),
        role: run.role,
        kind: run.kind,
        status,
        isRoot,
        startedTs: run.started_ts,
        finished: run.finished,
        // Actual run duration once finished (max member elapsed) — so a completed
        // stage shows a fixed time instead of `now - start` ticking up forever.
        durationMs: run.finished
          ? Math.max(0, ...(run.members || []).map((m) => m.elapsed_ms || 0))
          : null,
      },
    });
    for (const c of kids.get(run.run_id) || []) {
      edges.push({
        id: `${run.run_id}->${c.run_id}`,
        source: run.run_id,
        target: c.run_id,
        animated: runStatus(c) === "running",
        style: { stroke: runStatus(c) === "running" ? "var(--ac)" : "var(--line-3)" },
      });
      walk(c, depth + 1);
    }
  };
  walk(root, 0);
  return { nodes, edges };
}

// Small summary for the panel header.
export function summarize(snapshot, root) {
  const all = [...(snapshot.live || []), ...(snapshot.recent || [])];
  const stages = all.filter((r) => r.parent_run_id === root.run_id);
  const running = stages.filter((r) => runStatus(r) === "running").length;
  return {
    total: stages.length,
    running,
    manager: backendLabel(root) || "manager",
    live: !root.finished,
    cwd: root.cwd,
  };
}
