import { test } from "node:test";
import assert from "node:assert/strict";
import { applyChunks, applyPage, byteLength, TAIL_CHARS } from "./output.js";

test("contiguous chunks append; repeats are ignored", () => {
  let t = applyChunks({}, [{ step_id: "a.a1", offset: 0, text: "修正" }]);
  assert.deepEqual(t["a.a1"], { end: 6, text: "修正", gap: false });
  t = applyChunks(t, [
    { step_id: "a.a1", offset: 6, text: "OK" },
    { step_id: "a.a1", offset: 0, text: "修正" },
  ]);
  assert.deepEqual(t["a.a1"], { end: 8, text: "修正OK", gap: false });
  assert.equal(byteLength("修正OK"), 8);
});

test("a missed chunk marks a gap instead of splicing", () => {
  let t = applyChunks({}, [{ step_id: "a.a1", offset: 0, text: "abc" }]);
  t = applyChunks(t, [{ step_id: "a.a1", offset: 10, text: "xyz" }]);
  assert.equal(t["a.a1"].gap, true);
  assert.equal(t["a.a1"].end, 13);
  // Opening mid-stream is a gap too (the start is on disk).
  assert.equal(applyChunks({}, [{ step_id: "b.a1", offset: 50, text: "late" }])["b.a1"].gap, true);
});

test("a disk page fills a gap unless live output is already ahead", () => {
  let t = applyChunks({}, [{ step_id: "a.a1", offset: 10, text: "xyz" }]);
  t = applyPage(t, "a.a1", { offset: 0, next_offset: 13, size: 13, text: "0123456789xyz" });
  assert.deepEqual(t["a.a1"], { end: 13, text: "0123456789xyz", gap: false, truncated: false });
  const ahead = applyChunks(t, [{ step_id: "a.a1", offset: 13, text: "!!" }]);
  assert.equal(applyPage(ahead, "a.a1", { offset: 0, next_offset: 13, text: "old" }), ahead);
});

test("tails are bounded", () => {
  const big = "x".repeat(TAIL_CHARS + 50);
  const t = applyChunks({}, [{ step_id: "a.a1", offset: 0, text: big }]);
  assert.equal(t["a.a1"].text.length, TAIL_CHARS);
  assert.equal(t["a.a1"].end, TAIL_CHARS + 50);
});
