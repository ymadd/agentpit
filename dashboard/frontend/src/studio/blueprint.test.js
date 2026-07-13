import { test } from "node:test";
import assert from "node:assert/strict";
import { seedBlueprint, deriveEdges, edgesFor } from "./blueprint.js";
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
  assert.equal(edgesFor(bp).length, 10); // none stored → derived
  bp.edges = [{ id: "x", source: "goal", target: "s1", kind: "custom" }];
  assert.equal(edgesFor(bp).length, 1); // stored wins
  assert.equal(edgesFor(bp)[0].kind, "custom");
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
