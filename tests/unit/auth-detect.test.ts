import { describe, expect, it } from "vitest";

import {
  isAuthFailure,
  formatAuthFailureMessage,
} from "../../src/auth/detect-failure.js";

describe("isAuthFailure", () => {
  it.each([
    "stream error: Failed to refresh token: 401 Unauthorized",
    "ERROR: 401 Unauthorized",
    "Please log in to continue.",
    "Authentication failed",
    "Invalid API key",
    "no valid credentials",
    "Token expired",
  ])("recognizes %s as auth failure", (text) => {
    expect(isAuthFailure(text)).toBe(true);
  });

  it.each([
    "OK reply complete",
    "no stdout, exit=0",
    "Found 3 issues in src/foo.ts",
    "",
  ])("does not flag innocuous text: %s", (text) => {
    expect(isAuthFailure(text)).toBe(false);
  });
});

describe("formatAuthFailureMessage", () => {
  it("includes backend and login command", () => {
    const msg = formatAuthFailureMessage("codex", "codex login");
    expect(msg).toContain("[codex]");
    expect(msg).toContain("codex login");
  });

  it("appends launch message when provided", () => {
    const msg = formatAuthFailureMessage(
      "gemini",
      "gemini",
      "Opened Terminal",
    );
    expect(msg).toContain("Opened Terminal");
  });
});
