import { describe, expect, it } from "vitest";

import {
  buildAggregatorPrompt,
  renderConcatenatedOutput,
  type MemberOutcome,
} from "../../src/tools/ensemble.js";

const outcomes: MemberOutcome[] = [
  { backend: "gemini", transport: "exec", output: "Looks fine." },
  { backend: "opencode", transport: "acp", output: "Found 2 issues." },
  { backend: "claude", transport: "skipped", error: "auth missing" },
];

describe("renderConcatenatedOutput", () => {
  it("emits one section per outcome with source headers", () => {
    const text = renderConcatenatedOutput(outcomes);
    expect(text).toContain("=== gemini (transport=exec) ===");
    expect(text).toContain("Looks fine.");
    expect(text).toContain("=== opencode (transport=acp) ===");
    expect(text).toContain("Found 2 issues.");
    expect(text).toContain("=== claude (transport=skipped) ===");
    expect(text).toContain("[error] auth missing");
  });
});

describe("buildAggregatorPrompt", () => {
  it("includes the original prompt and each successful response", () => {
    const text = buildAggregatorPrompt("review src/", outcomes);
    expect(text).toContain("# Original task");
    expect(text).toContain("review src/");
    expect(text).toContain("## [gemini]");
    expect(text).toContain("Looks fine.");
    expect(text).toContain("## [opencode]");
    expect(text).toContain("Found 2 issues.");
  });

  it("marks failed members explicitly", () => {
    const text = buildAggregatorPrompt("review src/", outcomes);
    expect(text).toContain("## [claude] (failed)");
    expect(text).toContain("auth missing");
  });
});
