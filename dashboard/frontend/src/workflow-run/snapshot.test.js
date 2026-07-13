import { test } from "node:test";
import assert from "node:assert/strict";
import { pickWorkflow, buildStageGraph, runStatus, summarize } from "./snapshot.js";

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

test("runStatus collapses run/member status into running|done|failed", () => {
  assert.equal(runStatus({ finished: false }), "running");
  assert.equal(runStatus({ finished: true, status: "ok", members: [] }), "done");
  assert.equal(runStatus({ finished: true, status: "error", members: [] }), "failed");
  assert.equal(runStatus({ finished: true, status: "interrupted", members: [] }), "failed");
  assert.equal(runStatus({ finished: true, status: "ok", members: [{ status: "error" }] }), "failed");
});

test("pickWorkflow prefers a live workflow over recent", () => {
  assert.equal(pickWorkflow(snap()).run_id, "wf1");
  assert.equal(pickWorkflow({ live: [], recent: [] }), null);
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

test("summarize counts stages and running", () => {
  const s = snap();
  const info = summarize(s, pickWorkflow(s));
  assert.equal(info.total, 3);
  assert.equal(info.running, 1);
  assert.equal(info.live, true);
});
