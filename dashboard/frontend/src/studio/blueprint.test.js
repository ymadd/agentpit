import { test } from "node:test";
import assert from "node:assert/strict";
import { seedBlueprint, deriveEdges, edgesFor, workflowName, deriveFlow } from "./blueprint.js";
import { stepNodeData, metaFor } from "./backends.js";

test("deriveEdges mirrors the vanilla default flow (goal→steps→ghost + spawn sub-flow)", () => {
  const e = deriveEdges(seedBlueprint());
  // 1 goal→s1, 4 handoffs, 1 spawn, 2 subflow, 1 return, 1 grow = 10
  assert.equal(e.length, 10);
  const has = (source, target) => e.some((x) => x.source === source && x.target === target);
  assert.ok(has("goal", "s1"), "goal→s1");
  assert.ok(has("s1", "s2") && has("s4", "s5"), "handoffs chain");
  assert.ok(has("s4", "g1"), "self-spawn step feeds its gensteps");
  assert.ok(has("g3", "s5"), "gensteps return to the next step");
  assert.ok(has("s5", "ghost"), "last step → grow ghost");
});

test("edgesFor prefers stored edges over the derived seed", () => {
  const bp = seedBlueprint();
  assert.equal(edgesFor(bp).length, 10); // no edges array → derived seed
  bp.edges = [{ id: "x", source: "goal", target: "s1", kind: "custom" }];
  assert.equal(edgesFor(bp).length, 1); // stored wins
  assert.equal(edgesFor(bp)[0].kind, "custom");
  // regression: a deliberately-emptied edge set must persist as empty, not reseed
  bp.edges = [];
  assert.equal(edgesFor(bp).length, 0);
});

test("deriveFlow distils the drawn edges into an ordered step sequence", () => {
  // seed flow follows goal→s1→…→s5 (spawn sub-flow doesn't reorder the main steps)
  assert.equal(deriveFlow(seedBlueprint()), "Diagnose → Plan → Implement → Review → Integrate");
  // custom edges reorder the flow: reachable-from-goal first (in edge order),
  // then any unreached steps trail in array order
  const bp = seedBlueprint();
  bp.edges = [
    { id: "e1", source: "goal", target: "s2" },
    { id: "e2", source: "s2", target: "s1" },
  ];
  assert.equal(deriveFlow(bp), "Plan → Diagnose → Implement → Review → Integrate");
  assert.equal(deriveFlow({ steps: [] }), "");
});

test("workflowName namespaces types so they never collide with base", () => {
  assert.equal(workflowName(null), "base");
  assert.equal(workflowName({ name: "review", _key: 1 }), "type.review");
  assert.equal(workflowName({ name: "", _key: 7 }), "type-7"); // unnamed
  // regression: a type literally named "base" must not share base's sketch
  assert.notEqual(workflowName({ name: "base", _key: 2 }), "base");
});

test("stepNodeData resolves manager + role workers to visuals", () => {
  const roles = [{ name: "coder", backends: ["codex"] }];
  const st = { index: "02", name: "Plan", manager: "claude", dynamic: true, ask: false, persona: "p", behavior: "b", workers: [{ type: "role", id: "coder" }] };
  const d = stepNodeData(st, roles);
  assert.equal(d.managerMono, metaFor("claude").mono);
  assert.equal(d.managerColor, "#d98a6b");
  assert.equal(d.workers.length, 1);
  assert.equal(d.workers[0].label, "coder");
  assert.equal(d.workers[0].color, "#56b89a"); // codex color (role's primary backend)
  assert.equal(d.workers[0].known, true);
});

test("stepNodeData marks an unknown role worker as a faint example", () => {
  const d = stepNodeData({ manager: "claude", workers: [{ type: "role", id: "ghost-role" }] }, []);
  assert.equal(d.workers[0].known, false);
  assert.equal(d.workers[0].label, "ghost-role");
});
