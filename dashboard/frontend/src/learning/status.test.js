import assert from "node:assert/strict";
import test from "node:test";

import {
  donutSegments,
  evidenceCells,
  pct,
  scoreColor,
  timelineBars,
  age,
} from "./status.js";

test("scoreColor ramps upward and flattens below the floor", () => {
  const low = scoreColor(10);
  assert.equal(low, scoreColor(40), "everything under 40 shares the dim end");
  const [, hueLow, satLow] = low.match(/hsl\((\d+) (\d+)% (\d+)%\)/).map(Number);
  const [, hueHigh, satHigh] = scoreColor(100).match(/hsl\((\d+) (\d+)% (\d+)%\)/).map(Number);
  assert.ok(hueHigh < hueLow, "the top of the ramp is the teal end");
  assert.ok(satHigh > satLow, "high scores read as more saturated");
  // Out-of-range input must not produce a broken colour string.
  assert.match(scoreColor(140), /^hsl\(162 54% 40%\)$/);
});

test("pct never divides by zero", () => {
  assert.equal(pct(2, 3), 67);
  assert.equal(pct(0, 0), 0);
});

test("evidenceCells keeps only cells with telemetry, heaviest first", () => {
  const status = {
    rows: [
      {
        backend: "claude",
        cells: [
          { category: "coding", evidence: { labels: 2, projected: 40 } },
          { category: "docs" },
        ],
      },
      {
        backend: "codex",
        cells: [{ category: "review", evidence: { labels: 5, projected: 80 } }],
      },
    ],
  };
  const cells = evidenceCells(status);
  assert.equal(cells.length, 2, "cells without evidence are dropped");
  assert.deepEqual(
    cells.map((c) => [c.backend, c.cell.category]),
    [
      ["codex", "review"],
      ["claude", "coding"],
    ]
  );
});

test("donutSegments lay out in trust order without gaps", () => {
  const segments = donutSegments({ total: 40, benchmarked: 4, learned: 16, seeded: 20 }, 100);
  assert.deepEqual(
    segments.map((s) => [s.key, s.len, s.offset]),
    [
      ["benchmarked", 10, 0],
      ["learned", 40, -10],
      ["seeded", 50, -50],
    ]
  );
  // An empty matrix must not produce NaN lengths.
  assert.deepEqual(donutSegments({ total: 0 }, 100).map((s) => s.len), [0, 0, 0]);
});

test("timelineBars share one scale and survive an empty window", () => {
  const { max, bars } = timelineBars([
    { start_ms: 0, labels: 0, good: 0, bad: 0 },
    { start_ms: 1, labels: 4, good: 3, bad: 1 },
  ]);
  assert.equal(max, 4);
  assert.equal(bars[0].goodRatio, 0);
  assert.equal(bars[1].goodRatio, 0.75);
  assert.equal(bars[1].badRatio, 0.25);

  const quiet = timelineBars([{ start_ms: 0, labels: 0, good: 0, bad: 0 }]);
  assert.equal(quiet.max, 0);
  assert.equal(quiet.bars.length, 1, "a quiet window still renders its days");
  assert.equal(quiet.bars[0].goodRatio, 0);
});

test("age reports the largest coarse unit, and nothing for an unknown timestamp", () => {
  const now = 10 * 86_400_000;
  assert.equal(age(0, now), null);
  assert.equal(age(now - 30_000, now), "just now");
  assert.equal(age(now - 5 * 60_000, now), "5m");
  assert.equal(age(now - 3 * 3_600_000, now), "3h");
  assert.equal(age(now - 2 * 86_400_000, now), "2d");
});
