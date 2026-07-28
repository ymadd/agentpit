import { test } from "node:test";
import assert from "node:assert/strict";
import { seedBlueprint, deriveEdges, edgesFor, workflowName, deriveFlow, deriveSteps } from "./blueprint.js";
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

test("deriveSteps projects the cards to config shape in the drawn order", () => {
  const plan = deriveSteps(seedBlueprint());
  assert.deepEqual(plan.map((s) => s.name), ["Diagnose", "Plan", "Implement", "Review", "Integrate"]);

  const review = plan.find((s) => s.name === "Review");
  assert.equal(review.manager_backend, "claude");
  assert.deepEqual(review.roles, ["reviewer", "security"]);
  assert.deepEqual(review.backends, []);
  assert.equal(review.fanout, 3);
  assert.equal(review.dynamic, true);
  assert.equal(review.ask, true);
  assert.ok(review.persona.startsWith("Check spec violations"));

  // geometry, ids, the display index, and the canvas-only `spawns` flag stay out of config
  for (const key of ["id", "index", "x", "y", "w", "spawns", "workers"]) {
    assert.ok(!(key in review), `${key} must not reach config`);
  }
});

test("deriveSteps splits workers into cast roles and raw CLI backends", () => {
  const bp = seedBlueprint();
  bp.steps = [{ id: "s1", name: "Build", manager: "codex", workers: [{ type: "role", id: "coder" }, { type: "cli", id: "opencode" }] }];
  bp.edges = [{ id: "e", source: "goal", target: "s1" }];
  const [s] = deriveSteps(bp);
  assert.deepEqual(s.roles, ["coder"]);
  assert.deepEqual(s.backends, ["opencode"]);
});

test("deriveSteps drops unnamed cards and normalizes blanks to null", () => {
  const bp = seedBlueprint();
  bp.steps = [
    { id: "s1", name: "  ", manager: "claude", workers: [] },
    { id: "s2", name: " Ship ", manager: "", persona: "  ", behavior: "", fanout: 0, workers: [] },
  ];
  bp.edges = [{ id: "e", source: "goal", target: "s2" }];
  const plan = deriveSteps(bp);
  assert.equal(plan.length, 1, "the unnamed card is not a config entry");
  assert.equal(plan[0].name, "Ship");
  assert.equal(plan[0].persona, null);
  assert.equal(plan[0].behavior, null);
  assert.equal(plan[0].manager_backend, null);
  assert.equal(plan[0].fanout, null, "fanout 0 is not a real width");
});

// flow and steps come from one traversal, so they can never disagree about the order.
test("deriveSteps and deriveFlow always agree on the order", () => {
  const bp = seedBlueprint();
  bp.edges = [
    { id: "e1", source: "goal", target: "s2" },
    { id: "e2", source: "s2", target: "s1" },
  ];
  assert.equal(deriveSteps(bp).map((s) => s.name).join(" → "), deriveFlow(bp));
});
