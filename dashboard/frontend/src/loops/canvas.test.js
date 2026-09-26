import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import {
  layoutOf,
  toFlow,
  addNode,
  removeNode,
  updateNode,
  connect,
  removeEdge,
  setEdgeOn,
  moveNode,
  uniqueId,
  edgeOnFor,
  inputsOf,
  newBlueprint,
  NODE_W,
} from "./canvas.js";

const FIXTURE = JSON.parse(
  readFileSync(
    new URL("../../../../agentpit-events/tests/fixtures/loops/blueprint_fix_until_green.json", import.meta.url),
    "utf8"
  )
);

test("repeat children sit inside their group, and groups precede children", () => {
  const { nodes } = toFlow(FIXTURE);
  const ids = nodes.map((n) => n.id);
  assert.ok(ids.indexOf("fix") < ids.indexOf("implement"));
  const impl = nodes.find((n) => n.id === "implement");
  assert.equal(impl.parentId, "fix");
  assert.equal(impl.extent, "parent");
  const fix = nodes.find((n) => n.id === "fix");
  assert.equal(fix.type, "loopGroup");
  // The saved layout wins; the group still holds its children.
  assert.deepEqual(fix.position, { x: 320, y: 40 });
  const box = layoutOf(FIXTURE);
  for (const child of ["implement", "test"]) {
    const b = box.get(child);
    assert.ok(b.x + b.w <= box.get("fix").w, `${child} fits the group's width`);
    assert.ok(b.y + b.h <= box.get("fix").h, `${child} fits the group's height`);
  }
});

test("without a layout, a chain reads left to right", () => {
  const doc = {
    nodes: [
      { id: "c", kind: "agent" },
      { id: "a", kind: "agent" },
      { id: "b", kind: "check" },
    ],
    edges: [
      { from: "a", to: "b" },
      { from: "b", to: "c" },
    ],
  };
  const box = layoutOf(doc);
  assert.ok(box.get("a").x < box.get("b").x && box.get("b").x < box.get("c").x);
  assert.equal(box.get("a").w, NODE_W);
});

test("dangling or cyclic parents still draw at the top level", () => {
  const doc = {
    nodes: [
      { id: "orphan", kind: "agent", parent: "nope" },
      { id: "r1", kind: "repeat", parent: "r2" },
      { id: "r2", kind: "repeat", parent: "r1" },
    ],
    edges: [],
  };
  const { nodes } = toFlow(doc);
  assert.equal(nodes.length, 3);
  assert.ok(nodes.every((n) => !n.parentId));
});

test("the run overlay carries each node's phase, outcome, assignee and n/max", () => {
  const view = {
    nodes: [
      { node: "plan", kind: "agent", phase: "done", outcome: "ok", attempts: 1, runs: 1, assignee: { role: "planner", backend: "claude" } },
      { node: "fix", kind: "repeat", phase: "running", attempts: 1, runs: 1, iteration: { repeat: "fix", outer: [], n: 2, max: 4, open: true } },
      { node: "implement", kind: "agent", phase: "running", iter: [2], attempts: 1, runs: 2, step_id: "implement.i2.a1" },
    ],
  };
  const { nodes, edges } = toFlow(FIXTURE, view, "implement");
  const plan = nodes.find((n) => n.id === "plan");
  assert.equal(plan.data.run.phase, "done");
  assert.equal(plan.data.run.assignee.backend, "claude");
  assert.deepEqual(nodes.find((n) => n.id === "fix").data.run.iteration, { n: 2, max: 4, open: true });
  assert.equal(nodes.find((n) => n.id === "implement").selected, true);
  assert.equal(nodes.find((n) => n.id === "signoff").data.run, undefined);
  const planToFix = edges.find((e) => e.source === "plan");
  assert.match(planToFix.className, /lp-edge-fired/);
  const exhausted = edges.find((e) => e.label === "exhausted");
  assert.ok(exhausted && !/fired/.test(exhausted.className));
  const implToTest = edges.find((e) => e.source === "implement");
  assert.equal(implToTest.animated, true);
});

test("design edits keep keys the Studio does not know", () => {
  const doc = {
    ...FIXTURE,
    x_owner: "team-a",
    nodes: FIXTURE.nodes.map((n) => (n.id === "plan" ? { ...n, x_color: "teal" } : n)),
  };
  const edited = updateNode(doc, "plan", { title: "Plan it", role: "", model: undefined });
  const plan = edited.nodes.find((n) => n.id === "plan");
  assert.equal(plan.title, "Plan it");
  assert.equal(plan.x_color, "teal");
  assert.equal(plan.role, undefined, "an emptied field is removed");
  assert.equal(edited.x_owner, "team-a");
  assert.equal(doc.nodes.find((n) => n.id === "plan").title, "Plan", "the input is not mutated");
  // An emptied task stays (it is being typed).
  assert.equal(updateNode(doc, "plan", { task: "" }).nodes.find((n) => n.id === "plan").task, "");
});

test("adding, connecting and removing nodes", () => {
  let doc = newBlueprint("tiny");
  const added = addNode(doc, "check", { x: 300.4, y: 80.6 });
  doc = added.doc;
  assert.equal(added.id, "check");
  assert.deepEqual(doc.layout.check, { x: 300, y: 81 });
  doc = connect(doc, "work", "check");
  doc = connect(doc, "work", "check"); // no duplicate
  assert.equal(doc.edges.length, 1);
  assert.deepEqual(doc.edges[0], { from: "work", to: "check" });
  doc = setEdgeOn(doc, 0, "not_ok");
  assert.equal(doc.edges[0].on, "not_ok");
  doc = setEdgeOn(doc, 0, "ok");
  assert.equal(doc.edges[0].on, undefined);
  doc = removeEdge(doc, 0);
  assert.equal(doc.edges.length, 0);

  const rep = addNode(doc, "repeat");
  doc = rep.doc;
  const inner = addNode(doc, "agent", { parent: rep.id });
  doc = connect(inner.doc, "work", rep.id);
  doc = removeNode(doc, rep.id);
  assert.deepEqual(doc.nodes.map((n) => n.id), ["work", "check"]);
  assert.equal(doc.edges.length, 0);
  assert.equal(doc.layout[rep.id], undefined);
});

test("ids are unique, lowercase and start with a letter", () => {
  const doc = { nodes: [{ id: "agent" }, { id: "agent2" }] };
  assert.equal(uniqueId(doc, "agent"), "agent3");
  assert.equal(uniqueId(doc, "9 Lives"), "n9lives");
  assert.equal(uniqueId({}, ""), "node");
});

test("moving a node records whole pixels and keeps the rest of its layout", () => {
  const doc = moveNode({ layout: { fix: { x: 1, y: 2, w: 400, h: 200, x_pin: true } } }, "fix", { x: 10.2, y: 20.7 });
  assert.deepEqual(doc.layout.fix, { x: 10, y: 21, w: 400, h: 200, x_pin: true });
});

test("edge conditions follow the source kind, and inputs come from the document", () => {
  assert.ok(edgeOnFor("repeat").includes("exhausted"));
  assert.ok(!edgeOnFor("gate").includes("error"));
  assert.deepEqual(inputsOf(FIXTURE), [{ name: "goal", required: true, description: "What to fix", default: "" }]);
});

test("document patches and diagnostics by node", async () => {
  const { updateDoc, diagnosticsByNode } = await import("./canvas.js");
  const doc = updateDoc({ name: "x", title: "T", x_keep: 1 }, { title: undefined, description: "d" });
  assert.deepEqual(doc, { name: "x", x_keep: 1, description: "d" });
  const by = diagnosticsByNode(FIXTURE, [
    { severity: "error", path: "nodes[1].max_iterations", message: "a" },
    { severity: "warning", path: "edges[0].to", node: "signoff", message: "b" },
    { severity: "error", path: "budget.max_steps", message: "c" },
  ]);
  assert.equal(by.get("fix")[0].message, "a");
  assert.equal(by.get("signoff")[0].message, "b");
  assert.equal(by.get("")[0].message, "c");
});

test("a loop's blueprint file maps back to a store reference", async () => {
  const { fileRefOf } = await import("./canvas.js");
  assert.deepEqual(fileRefOf({ scope: "project", path: "/r/app/.agentpit/blueprints/fix.json" }), {
    scope: "project",
    name: "fix",
    project: "/r/app",
  });
  assert.deepEqual(fileRefOf({ scope: "user", path: "/home/u/.config/agentpit/blueprints/x-1.json" }), {
    scope: "user",
    name: "x-1",
    project: null,
  });
  assert.equal(fileRefOf({ scope: "project", path: "/tmp/elsewhere/fix.json" }), null);
  assert.equal(fileRefOf({ scope: "inline" }), null);
});
