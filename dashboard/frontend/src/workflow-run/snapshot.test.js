import { test } from "node:test";
import assert from "node:assert/strict";
import { pickWorkflow, listWorkflows, buildStageGraph, runStatus, summarize, descendants } from "./snapshot.js";

// A manager (claude) that dispatched three stages: codex failed, codex ok, and
// an antigravity reviewer still running — the shape a real `agentpit workflow`
// run produces in the event log.
const snap = () => ({
  live: [
    { run_id: "wf1", kind: "workflow", cwd: "/p/agentpit", started_ts: 1000, finished: false, status: null, members: [{ backend: "claude", status: "running" }], parent_run_id: null, depth: 0, role: null },
    { run_id: "s3", kind: "rescue", cwd: "/p", started_ts: 4000, finished: false, status: null, members: [{ backend: "antigravity", status: "running" }], parent_run_id: "wf1", depth: 1, role: "reviewer" },
  ],
  recent: [
    { run_id: "s1", kind: "rescue", started_ts: 1500, finished: true, status: "error", members: [{ backend: "codex", status: "error", elapsed_ms: 5000 }], parent_run_id: "wf1", depth: 1, role: null },
    { run_id: "s2", kind: "rescue", started_ts: 2500, finished: true, status: "ok", members: [{ backend: "codex", status: "ok", elapsed_ms: 12000 }], parent_run_id: "wf1", depth: 1, role: null },
  ],
});

// wf1 dispatches s1..s3; s3 self-spawns two adversaries (g1, g2). The nesting the
// tidy layout has to keep together.
const nested = () => {
  const s = snap();
  s.live.push(
    { run_id: "g1", kind: "rescue", started_ts: 5000, finished: false, status: null, members: [{ backend: "codex", status: "running" }], parent_run_id: "s3", depth: 2, role: "adversary" },
    { run_id: "g2", kind: "rescue", started_ts: 6000, finished: false, status: null, members: [{ backend: "claude", status: "running" }], parent_run_id: "s3", depth: 2, role: "refuter" }
  );
  return s;
};

test("runStatus collapses run/member status into running|done|failed", () => {
  assert.equal(runStatus({ finished: false }), "running");
  assert.equal(runStatus({ finished: true, status: "ok", members: [] }), "done");
  assert.equal(runStatus({ finished: true, status: "error", members: [] }), "failed");
  assert.equal(runStatus({ finished: true, status: "interrupted", members: [] }), "failed");
  assert.equal(runStatus({ finished: true, status: "ok", members: [{ status: "error" }] }), "failed");
});

// Tracker::snapshot marks a run whose process died `interrupted` but leaves `finished`
// false — testing `!finished` alone rendered it as perpetually running.
test("runStatus treats an interrupted-but-unfinished run as failed", () => {
  assert.equal(runStatus({ finished: false, status: "interrupted", members: [] }), "failed");
});

test("pickWorkflow prefers a live workflow over recent", () => {
  assert.equal(pickWorkflow(snap()).run_id, "wf1");
  assert.equal(pickWorkflow({ live: [], recent: [] }), null);
});

test("listWorkflows returns live roots first, then newest, de-duped", () => {
  const s = snap();
  const dead = { run_id: "wf0", kind: "workflow", started_ts: 9999, finished: false, status: "interrupted", members: [], parent_run_id: null, depth: 0 };
  s.recent.push(dead);
  // the live copy of wf1 also lingers in `recent` across a poll transition
  s.recent.push({ ...s.live[0] });
  const ids = listWorkflows(s).map((w) => w.run_id);
  // wf0 started later but is not live, so the live wf1 still sorts first
  assert.deepEqual(ids, ["wf1", "wf0"]);
});

test("buildStageGraph nests dispatched sub-runs under the manager", () => {
  const s = snap();
  const g = buildStageGraph(s, pickWorkflow(s));
  assert.equal(g.nodes.length, 4);
  assert.equal(g.edges.length, 3);
  const d = Object.fromEntries(g.nodes.map((n) => [n.id, n.data]));

  assert.equal(d.wf1.title, "WORKFLOW");
  assert.equal(d.wf1.status, "running");
  // role labels the node when present; otherwise the run kind
  assert.equal(d.s3.title, "reviewer");
  assert.equal(d.s1.title, "rescue");

  // a finished stage carries its actual duration; a running one does not
  assert.equal(d.s1.status, "failed");
  assert.equal(d.s1.durationMs, 5000);
  assert.equal(d.s2.status, "done");
  assert.equal(d.s2.durationMs, 12000);
  assert.equal(d.s3.status, "running");
  assert.equal(d.s3.durationMs, null);

  // only edges into a running stage animate
  assert.equal(g.edges.find((e) => e.target === "s3").animated, true);
  assert.equal(g.edges.find((e) => e.target === "s1").animated, false);
});

test("buildStageGraph exposes the run for the detail panel", () => {
  const s = snap();
  const g = buildStageGraph(s, pickWorkflow(s));
  const s1 = g.nodes.find((n) => n.id === "s1");
  assert.equal(s1.data.run.members[0].backend, "codex");
});

// The layout has to place a self-spawned subtree BESIDE its parent. The previous
// per-depth row counter numbered rows in visit order, so a grandchild landed at the
// top of the canvas while its parent sat at the bottom.
test("buildStageGraph centres a parent on its children and keeps subtrees contiguous", () => {
  const s = nested();
  const g = buildStageGraph(s, pickWorkflow(s));
  const y = Object.fromEntries(g.nodes.map((n) => [n.id, n.position.y]));
  const x = Object.fromEntries(g.nodes.map((n) => [n.id, n.position.x]));

  // depth drives x: root < stages < self-spawned sub-stages
  assert.ok(x.wf1 < x.s3 && x.s3 < x.g1);
  assert.equal(x.g1, x.g2);

  // s3 sits exactly between the two children it spawned
  assert.equal(y.s3, (y.g1 + y.g2) / 2);
  // the manager sits between its first and last stage
  assert.equal(y.wf1, (y.s1 + y.s3) / 2);
  // s1/s2 are laid out above the s3 subtree, which owns a contiguous band
  assert.ok(y.s1 < y.s2 && y.s2 < y.g1 && y.g1 < y.g2);
});

test("summarize counts stages and running", () => {
  const s = snap();
  const info = summarize(s, pickWorkflow(s));
  assert.equal(info.total, 3);
  assert.equal(info.running, 1);
  assert.equal(info.failed, 1);
  assert.equal(info.live, true);
});

// The canvas draws the whole dispatch subtree, so the header must count it too —
// counting only direct children said "3 stages" over a 5-node graph.
test("summarize counts every drawn stage, not just direct children", () => {
  const s = nested();
  const root = pickWorkflow(s);
  assert.equal(descendants(s, root).length, 5);
  assert.equal(summarize(s, root).total, 5);
  assert.equal(summarize(s, root).running, 3);
});

// An interrupted root lives in `recent` with finished:false — it must not read as live.
test("summarize marks a run that is not in the live bucket as finished", () => {
  const s = { live: [], recent: [{ run_id: "wf9", kind: "workflow", started_ts: 1, finished: false, status: "interrupted", members: [{ backend: "codex", status: "interrupted", elapsed_ms: 300000 }], parent_run_id: null, depth: 0 }] };
  const info = summarize(s, pickWorkflow(s));
  assert.equal(info.live, false);
  assert.equal(buildStageGraph(s, pickWorkflow(s)).nodes[0].data.status, "failed");
});

// A malformed parent chain (two parents claiming one run) must not duplicate a subtree.
test("buildStageGraph places each run once even on a cyclic parent chain", () => {
  const s = {
    live: [
      { run_id: "a", kind: "workflow", started_ts: 1, finished: false, status: null, members: [], parent_run_id: "b", depth: 0 },
      { run_id: "b", kind: "rescue", started_ts: 2, finished: false, status: null, members: [], parent_run_id: "a", depth: 1 },
    ],
    recent: [],
  };
  const g = buildStageGraph(s, s.live[0]);
  assert.equal(g.nodes.length, 2);
  assert.equal(new Set(g.nodes.map((n) => n.id)).size, 2);
});
