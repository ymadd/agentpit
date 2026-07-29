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
//
// NOTE the interrupted case: `Tracker::snapshot` labels a run whose process died
// without a run_finished event `status: "interrupted"` but leaves `finished` FALSE
// (there was no finish event to record). Testing `!finished` alone would therefore
// render a dead stage as perpetually running, with a ticking timer — check the
// terminal statuses first.
// "profile coding 82" (reason + category + score) or just "explicit" when the route
// carries no diagnosis detail — the router's own decision, from `Event::RouteDecided`
// via the snapshot's `run.route`. A score of 0 is real data and must survive.
export function routeLabel(route) {
  return [route.reason, route.category, route.score].filter((v) => v != null).join(" ");
}

export function runStatus(run) {
  const s = run.status;
  if (s === "error" || s === "interrupted") return "failed";
  if (!run.finished) return "running";
  if ((run.members || []).some((m) => m.status === "error")) return "failed";
  return "done";
}

// Index the snapshot's two buckets into one run map plus the set of run_ids the
// backend still considers LIVE (process alive, no run_finished). A run can appear in
// both buckets across a poll transition, so the live copy wins.
function indexRuns(snapshot) {
  const live = snapshot.live || [];
  const recent = snapshot.recent || [];
  const liveIds = new Set(live.map((r) => r.run_id));
  const byId = new Map();
  for (const r of [...live, ...recent]) {
    if (!byId.has(r.run_id)) byId.set(r.run_id, r);
  }
  return { byId, liveIds };
}

// Every workflow root in the snapshot — the run picker's options. Live runs first
// (that is what you want to watch), then newest-started.
export function listWorkflows(snapshot) {
  const { byId, liveIds } = indexRuns(snapshot);
  return [...byId.values()]
    .filter((r) => r.kind === "workflow")
    .sort((a, b) => {
      const al = liveIds.has(a.run_id) ? 1 : 0;
      const bl = liveIds.has(b.run_id) ? 1 : 0;
      if (al !== bl) return bl - al;
      return b.started_ts - a.started_ts;
    });
}

// Pick the workflow to visualize: a live (still-running) workflow if any,
// otherwise the most recently started workflow so you can review the last run.
export function pickWorkflow(snapshot) {
  return listWorkflows(snapshot)[0] || null;
}

function backendLabel(run) {
  const names = (run.members || []).map((m) => m.backend).filter(Boolean);
  const uniq = [...new Set(names)];
  return uniq.join(", ");
}

// Actual run duration once finished (max member elapsed) — so a completed stage shows a
// fixed time instead of `now - start` ticking up forever.
function durationMs(run) {
  const elapsed = (run.members || []).map((m) => m.elapsed_ms || 0);
  return elapsed.length ? Math.max(0, ...elapsed) : 0;
}

// Walk the dispatch tree rooted at `root`, assigning each run a (depth, row) slot.
//
// Rows come from a post-order leaf walk: every leaf takes the next free row and each
// parent centres on its children, so a subtree occupies one contiguous band and a child
// always renders beside its parent. (The previous per-depth counter numbered rows in
// visit order, which scattered grandchildren far from their parents and produced long
// crossing edges.) Returns runs in a stable depth→row order plus the parent→child pairs.
function walkTree(root, kids) {
  const placed = [];
  const links = [];
  const seen = new Set();
  let nextRow = 0;

  const place = (run, depth) => {
    seen.add(run.run_id);
    // Reserve children up front so a run reachable from two parents (a malformed or
    // pruned chain) is laid out once instead of duplicating a whole subtree.
    const children = (kids.get(run.run_id) || []).filter((c) => !seen.has(c.run_id));
    for (const c of children) seen.add(c.run_id);

    let row;
    if (children.length === 0) {
      row = nextRow++;
    } else {
      const rows = children.map((c) => {
        links.push([run, c]);
        return place(c, depth + 1);
      });
      row = (rows[0] + rows[rows.length - 1]) / 2;
    }
    placed.push({ run, depth, row });
    return row;
  };

  place(root, 0);
  placed.sort((a, b) => a.depth - b.depth || a.row - b.row);
  return { placed, links };
}

// Group every run under its parent, dispatch order first.
function childrenByParent(byId) {
  const kids = new Map();
  for (const r of byId.values()) {
    const p = r.parent_run_id;
    if (p && byId.has(p)) {
      if (!kids.has(p)) kids.set(p, []);
      kids.get(p).push(r);
    }
  }
  for (const list of kids.values()) list.sort((a, b) => a.started_ts - b.started_ts);
  return kids;
}

// Every run dispatched beneath `root`, excluding the root itself — exactly the stages
// the graph draws, so header counts and canvas never disagree. A plain collect: the
// canvas layout math in `walkTree` would be computed and thrown away here.
export function descendants(snapshot, root) {
  const kids = childrenByParent(indexRuns(snapshot).byId);
  const out = [];
  const seen = new Set([root.run_id]);
  const walk = (run) => {
    for (const c of kids.get(run.run_id) || []) {
      if (seen.has(c.run_id)) continue;
      seen.add(c.run_id);
      out.push(c);
      walk(c);
    }
  };
  walk(root);
  return out;
}

// Build React Flow {nodes, edges} for the workflow rooted at `root`.
export function buildStageGraph(snapshot, root) {
  const { byId } = indexRuns(snapshot);
  const { placed, links } = walkTree(root, childrenByParent(byId));

  const COL = 260;
  const ROW = 132;

  const nodes = placed.map(({ run, depth, row }) => {
    const isRoot = run.run_id === root.run_id;
    const status = runStatus(run);
    const finished = status !== "running";
    return {
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
        finished,
        durationMs: finished ? durationMs(run) : null,
        // The full run, for the detail panel (members, per-backend errors, cwd).
        run,
      },
    };
  });

  const edges = links.map(([parent, child]) => {
    const running = runStatus(child) === "running";
    return {
      id: `${parent.run_id}->${child.run_id}`,
      source: parent.run_id,
      target: child.run_id,
      animated: running,
      style: { stroke: running ? "var(--ac)" : "var(--line-3)" },
    };
  });

  return { nodes, edges };
}

// Small summary for the panel header. `total` counts every stage DRAWN (the whole
// dispatch subtree, not just direct children) so the header matches the canvas.
export function summarize(snapshot, root) {
  const stages = descendants(snapshot, root);
  let running = 0;
  let failed = 0;
  for (const r of stages) {
    const s = runStatus(r);
    if (s === "running") running += 1;
    else if (s === "failed") failed += 1;
  }
  return {
    total: stages.length,
    running,
    failed,
    manager: backendLabel(root) || "manager",
    // Liveness comes from the backend's own bucketing, not from `finished` — see runStatus.
    live: (snapshot.live || []).some((r) => r.run_id === root.run_id),
    cwd: root.cwd,
  };
}
