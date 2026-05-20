import { mkdtemp, writeFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { loadConfig } from "../../src/config.js";

describe("loadConfig", () => {
  let dir: string;

  beforeEach(async () => {
    dir = await mkdtemp(join(tmpdir(), "agentpit-config-"));
  });
  afterEach(async () => {
    await rm(dir, { recursive: true, force: true });
  });

  it("returns defaults when config file is absent", async () => {
    const result = await loadConfig(join(dir, "absent.toml"));
    expect(result.source).toBe("defaults");
    expect(result.config.default.backend).toBe("gemini");
    expect(result.config.ensemble.review_members).toEqual([
      "gemini",
      "opencode",
    ]);
  });

  it("parses TOML overrides", async () => {
    const path = join(dir, "config.toml");
    await writeFile(
      path,
      `
[default]
backend = "claude"
auto_route = false

[ensemble]
review_members = ["claude", "gemini"]
`,
    );
    const result = await loadConfig(path);
    expect(result.source).toBe("file");
    expect(result.config.default.backend).toBe("claude");
    expect(result.config.default.auto_route).toBe(false);
    expect(result.config.ensemble.review_members).toEqual([
      "claude",
      "gemini",
    ]);
  });

  it("expands ${ENV} references in strings", async () => {
    process.env.TEST_BACKEND = "opencode";
    const path = join(dir, "env.toml");
    await writeFile(
      path,
      `
[default]
backend = "\${TEST_BACKEND}"
`,
    );
    try {
      const result = await loadConfig(path);
      expect(result.config.default.backend).toBe("opencode");
    } finally {
      delete process.env.TEST_BACKEND;
    }
  });

  it("rejects invalid backend ids in route table", async () => {
    const path = join(dir, "bad.toml");
    await writeFile(
      path,
      `
[routes]
review = "imaginary-backend"
`,
    );
    await expect(loadConfig(path)).rejects.toThrow(/Failed to load/);
  });
});
