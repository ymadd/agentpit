import { describe, expect, it } from "vitest";

import { resolveTransport } from "../../src/dispatch.js";
import type { ExecAdapter } from "../../src/exec/base.js";
import type { BaseAdapter } from "../../src/adapters/base.js";

const dummyExec: ExecAdapter = {
  id: "gemini",
  buildSpec: () => ({ command: "true", args: [] }),
  run: async () => ({ output: "", exitCode: 0, signal: null }),
};

const dummyAcp = {
  id: "opencode",
} as unknown as BaseAdapter;

describe("resolveTransport", () => {
  it("returns exec when only exec is registered", () => {
    expect(
      resolveTransport("gemini", { execs: { gemini: dummyExec }, acps: {} }),
    ).toBe("exec");
  });

  it("returns acp when only acp is registered", () => {
    expect(
      resolveTransport("opencode", {
        execs: {},
        acps: { opencode: dummyAcp },
      }),
    ).toBe("acp");
  });

  it("prefers exec when both are registered for the same backend", () => {
    expect(
      resolveTransport("gemini", {
        execs: { gemini: dummyExec },
        acps: { gemini: dummyAcp },
      }),
    ).toBe("exec");
  });

  it("returns null when no transport exists", () => {
    expect(resolveTransport("goose", { execs: {}, acps: {} })).toBeNull();
  });
});
