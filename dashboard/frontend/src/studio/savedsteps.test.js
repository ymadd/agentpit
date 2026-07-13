import { test } from "node:test";
import assert from "node:assert/strict";
import { stepTemplate, stepFromTemplate, maxStepSeq } from "./savedsteps.js";

test("stepTemplate keeps semantic fields, drops geometry/id", () => {
  const t = stepTemplate({
    id: "s3",
    index: "03",
    x: 900,
    y: 200,
    w: 250,
    name: "Implement",
    manager: "codex",
    persona: "p",
    behavior: "b",
    dynamic: false,
    ask: true,
    fanout: 4,
    workers: [{ type: "role", id: "coder", extra: "drop-me" }],
  });
  assert.deepEqual(Object.keys(t).sort(), ["ask", "behavior", "dynamic", "fanout", "manager", "name", "persona", "workers"]);
  assert.equal(t.manager, "codex");
  assert.equal(t.dynamic, false);
  assert.equal(t.ask, true);
  // workers keep only type/id
  assert.deepEqual(t.workers, [{ type: "role", id: "coder" }]);
});

test("stepTemplate applies defaults for a sparse step", () => {
  const t = stepTemplate({ name: "New step" });
  assert.equal(t.manager, "claude");
  assert.equal(t.dynamic, true); // undefined → true
  assert.equal(t.ask, false);
  assert.equal(t.fanout, 2);
  assert.deepEqual(t.workers, []);
});

test("maxStepSeq seeds the drop counter so ids survive a remount without colliding", () => {
  assert.equal(maxStepSeq({ steps: [{ id: "s1" }, { id: "st-3" }, { id: "st-1" }] }), 3);
  assert.equal(maxStepSeq({ steps: [] }), 0);
  assert.equal(maxStepSeq(null), 0);
  // a reload re-seeds from the persisted max, so the next drop is unique
  const bp = { steps: [{ id: "st-2" }] };
  const next = stepFromTemplate({ name: "x" }, null, 2, maxStepSeq(bp) + 1);
  assert.equal(next.id, "st-3"); // not "st-1" (which would collide)
});

test("stepFromTemplate places a fresh step with a collision-free id + padded index", () => {
  const s = stepFromTemplate({ name: "Plan", manager: "claude" }, { x: 640.7, y: 210.2 }, 3, 12);
  assert.equal(s.id, "st-12"); // "st-" prefix never collides with seed "s1".."s5"
  assert.equal(s.index, "03");
  assert.equal(s.x, 641);
  assert.equal(s.y, 210);
  assert.equal(s.w, 250);
  assert.equal(s.name, "Plan");
  // index 10+ is not zero-padded
  assert.equal(stepFromTemplate({}, null, 10, 1).index, "10");
});
