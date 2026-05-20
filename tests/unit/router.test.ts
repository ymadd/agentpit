import { describe, expect, it } from "vitest";

import { Router } from "../../src/router.js";
import type { HubConfig } from "../../src/config.js";
import type { BackendId } from "../../src/types.js";

const baseConfig: HubConfig = {
  default: { backend: "gemini", auto_route: true },
  routes: {
    rescue: "gemini",
    review: "claude",
    explain: "gemini",
    refactor: "claude",
  },
  auto_route: {
    long_context_threshold: 100,
    long_context_backend: "gemini",
    review_keywords: ["audit", "review"],
    review_backend: "claude",
  },
  ensemble: {
    default_members: ["gemini", "claude", "opencode"],
    aggregator: undefined,
    review_members: ["gemini", "opencode"],
    review_aggregator: undefined,
  },
};

const available = new Set<BackendId>(["gemini", "claude", "opencode"]);

describe("Router.resolve", () => {
  it("honors explicit backend when registered", () => {
    const r = new Router(baseConfig, available);
    expect(
      r.resolve({ tool: "rescue", explicitBackend: "claude", task: "x" }),
    ).toEqual({ backend: "claude", reason: "explicit" });
  });

  it("ignores explicit backend if not available, falls through to route table", () => {
    const r = new Router(baseConfig, new Set<BackendId>(["gemini"]));
    expect(
      r.resolve({ tool: "rescue", explicitBackend: "claude", task: "x" }),
    ).toEqual({ backend: "gemini", reason: "route_table" });
  });

  it("uses route table for the requested tool", () => {
    const r = new Router(baseConfig, available);
    expect(r.resolve({ tool: "review", task: "x" })).toEqual({
      backend: "claude",
      reason: "route_table",
    });
  });

  it("auto-routes to long-context backend when task exceeds threshold", () => {
    const r = new Router(
      { ...baseConfig, routes: {} },
      available,
    );
    const longTask = "x".repeat(10_000); // > 100 / 4 = 25 tokens estimate threshold
    expect(r.resolve({ tool: "rescue", task: longTask })).toEqual({
      backend: "gemini",
      reason: "auto_long_context",
    });
  });

  it("auto-routes by review keyword when no route table entry", () => {
    const r = new Router(
      { ...baseConfig, routes: {} },
      available,
    );
    expect(
      r.resolve({ tool: "rescue", task: "please audit this function" }),
    ).toEqual({ backend: "claude", reason: "auto_keyword" });
  });

  it("falls back to default backend when nothing matches", () => {
    const r = new Router(
      { ...baseConfig, routes: {}, default: { backend: "opencode", auto_route: false } },
      available,
    );
    expect(r.resolve({ tool: "rescue", task: "hi" })).toEqual({
      backend: "opencode",
      reason: "default",
    });
  });
});
