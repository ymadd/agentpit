import { test } from "node:test";
import assert from "node:assert/strict";
import { listSketches, blueprintNameFor, sketchToBlueprint } from "./import.js";
import { seedBlueprint, BLUEPRINT_KEY } from "../studio/blueprint.js";

function storage(entries) {
  const keys = Object.keys(entries);
  return {
    length: keys.length,
    key: (i) => keys[i],
    getItem: (k) => entries[k] ?? null,
  };
}

test("only drawn sketches are offered", () => {
  const list = listSketches(
    storage({
      [`${BLUEPRINT_KEY}.type.review`]: JSON.stringify(seedBlueprint()),
      [`${BLUEPRINT_KEY}.base`]: "not json",
      "agentpit.lang": "ja",
      [`${BLUEPRINT_KEY}.type.empty`]: JSON.stringify({ steps: [] }),
    })
  );
  assert.deepEqual(list.map((s) => s.workflow), ["type.review"]);
  assert.equal(list[0].steps, 5);
});

test("names become valid blueprint file names", () => {
  assert.equal(blueprintNameFor("type.review"), "review");
  assert.equal(blueprintNameFor("base"), "base");
  assert.equal(blueprintNameFor("type-k3"), "k3");
  assert.equal(blueprintNameFor("type.Bug Fix!"), "bug-fix");
  assert.equal(blueprintNameFor("type.__"), "sketch-__");
  assert.equal(blueprintNameFor(""), "sketch-base");
});

test("a sketch becomes a linear chain with gates, manager nodes and warnings", () => {
  const { doc, warnings } = sketchToBlueprint(seedBlueprint(), "seed");
  const kinds = doc.nodes.map((n) => `${n.id}:${n.kind}`);
  assert.deepEqual(kinds, [
    "diagnose:agent",
    "plan:manager",
    "implement:manager",
    "implement-ok:gate",
    "review:manager",
    "review-ok:gate",
    "integrate:agent",
    "integrate-ok:gate",
  ]);
  // A straight chain through every node.
  assert.equal(doc.edges.length, doc.nodes.length - 1);
  for (let i = 0; i < doc.edges.length; i++) {
    assert.deepEqual(doc.edges[i], { from: doc.nodes[i].id, to: doc.nodes[i + 1].id });
  }
  const diagnose = doc.nodes[0];
  assert.equal(diagnose.role, "longctx");
  assert.equal(diagnose.backend, undefined, "a role already picks the backend");
  assert.match(diagnose.task, /^Goal: \{\{goal\}\}/);
  assert.match(diagnose.task, /Classify the task/);
  assert.equal(doc.nodes.find((n) => n.id === "integrate").backend, "codex");
  assert.ok(warnings.some((w) => /fanout 3/.test(w)));
  assert.ok(warnings.some((w) => /Review: the self-spawning sub-swarm/.test(w)));
  assert.ok(warnings.some((w) => /Implement: only the first role/.test(w)));
  assert.equal(doc.inputs.goal.description, "Fix the auth flow");
  assert.ok(Object.keys(doc.layout).length === doc.nodes.length);
});
