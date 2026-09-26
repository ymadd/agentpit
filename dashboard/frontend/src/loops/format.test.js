import { test } from "node:test";
import assert from "node:assert/strict";
import {
  shortId,
  stateOf,
  stageText,
  assignees,
  duration,
  budgetUse,
  boardSections,
  gateOptions,
  inboxItems,
  answeredText,
  launcherCounts,
} from "./format.js";

const summary = (over = {}) => ({
  loop_id: "lp-0199a1b2c3d47e5f8a9b0c1d2e3f4a5b",
  title: "Fix",
  blueprint: { name: "fix-until-green", rev: "b1-x" },
  status: "running",
  usage: { steps: 3, active_ms: 90_000 },
  budget: { max_steps: 12, max_active_secs: 7200, max_parallel: 1 },
  head_seq: 10,
  updated_ts: 1000,
  writable: true,
  ...over,
});

test("a loop's state reads the summary and its runner", () => {
  assert.equal(stateOf({ summary: summary(), runner: "live" }), "running");
  assert.equal(stateOf({ summary: summary({ waiting: true }), runner: "live" }), "waiting");
  assert.equal(stateOf({ summary: summary({ waiting: true }), runner: "absent" }), "parked");
  assert.equal(stateOf({ summary: summary(), runner: "absent" }), "stalled");
  assert.equal(stateOf({ summary: summary({ pause: "drain" }), runner: "live" }), "paused");
  assert.equal(stateOf({ summary: summary({ status: "succeeded" }), runner: "absent" }), "succeeded");
  assert.equal(stateOf({ summary: summary({ status: "created" }), runner: "absent" }), "created");
  assert.equal(shortId("lp-0199a1b2c3d47e5f8a9b0c1d2e3f4a5b"), "2e3f4a5b");
});

test("the stage names the open iterations and who holds each running node", () => {
  const s = summary({
    iterations: [
      { repeat: "fix", outer: [], n: 2, max: 4, open: true },
      { repeat: "old", outer: [], n: 1, max: 2, open: false },
    ],
    active: [
      { step_id: "implement.i2.a1", node: "implement", iter: [2], kind: "agent", attempt: 1, assignee: { role: "coder", backend: "claude" }, started_ts: 1 },
      { step_id: "signoff.a1", node: "signoff", kind: "gate", attempt: 1, started_ts: 1 },
    ],
  });
  assert.equal(stageText(s), "fix 2/4 · implement#2 (coder) · ? signoff");
  assert.deepEqual(assignees(s), [{ node: "implement", kind: "agent", who: "coder · claude", model: null }]);
  assert.equal(
    stageText(summary({ status: "failed", finish: { reason: "unhandled_outcome", node: "test" } })),
    "unhandled_outcome @ test"
  );
});

test("durations and budget bars", () => {
  assert.equal(duration(5_000), "5s");
  assert.equal(duration(125_000), "2m05s");
  assert.equal(duration(3_900_000), "1h05m");
  assert.equal(duration(undefined), "");
  const b = budgetUse(summary());
  assert.equal(b.steps, 0.25);
  assert.equal(b.stepsText, "3/12");
  assert.ok(Math.abs(b.time - 90 / 7200) < 1e-9);
});

test("the board puts loops that need a person first and finished ones apart", () => {
  const rows = [
    { summary: summary({ loop_id: "lp-a", updated_ts: 5 }), runner: "live" },
    { summary: summary({ loop_id: "lp-b", waiting: true, updated_ts: 1 }), runner: "absent" },
    { summary: summary({ loop_id: "lp-c", status: "succeeded", updated_ts: 9 }), runner: "absent" },
    { summary: summary({ loop_id: "lp-d", status: "failed", updated_ts: 10 }), runner: "absent" },
  ];
  const { active, finished } = boardSections(rows);
  assert.deepEqual(active.map((x) => x.row.summary.loop_id), ["lp-b", "lp-a"]);
  assert.deepEqual(finished.map((x) => x.row.summary.loop_id), ["lp-d", "lp-c"]);
});

test("gates always have choices", () => {
  assert.deepEqual(gateOptions({ options: [] }).map((o) => o.id), ["approve", "reject"]);
  assert.deepEqual(gateOptions({ options: [{ id: "retry" }] }), [{ id: "retry", label: "retry", outcome: null }]);
});

test("the inbox merges loop gates with asks, soonest deadline first", () => {
  const items = inboxItems(
    {
      gates: [
        { loop_id: "lp-a", loop_title: "A", blueprint: "bp", parked: true, gate_id: "g1", kind: "approval", prompt: "ok?", options: [] },
        { loop_id: "lp-b", loop_title: "B", blueprint: "bp", parked: false, gate_id: "g2", kind: "budget", prompt: "more?", options: [{ id: "extend", label: "Extend" }], deadline_ms: 5000 },
      ],
    },
    [{ askId: "ask1", runId: "r", ts: 100, prompt: "which?", options: [], kind: "blocking", timeoutSecs: 1 }],
    3000
  );
  assert.deepEqual(items.map((i) => i.key), ["ask:ask1", "gate:lp-b:g2", "gate:lp-a:g1"]);
  assert.equal(items[0].overdue, true, "its 1s timeout ran out at 1100");
  assert.equal(items[1].overdue, false);
  assert.deepEqual(items[0].options.map((o) => o.id), ["yes", "no"]);
  assert.equal(items[2].parked, true);
  assert.equal(items[1].options[0].label, "Extend");
});

test("answered gates say who answered and how", () => {
  assert.equal(
    answeredText({ option: "approve", option_label: "Approve", by: { kind: "human", client: "agentpit/0.3" } }),
    "Approve — agentpit/0.3"
  );
  assert.equal(answeredText({ option: "reject", by: { kind: "timeout" }, comment: "late" }), "reject — timeout: late");
});

test("launcher counts running loops and every waiting decision", () => {
  const rows = [
    { summary: summary(), runner: "live" },
    { summary: summary({ waiting: true }), runner: "absent" },
  ];
  assert.deepEqual(launcherCounts(rows, { gates: [{}], hidden: 2 }, [{}]), { running: 1, decisions: 4 });
});
