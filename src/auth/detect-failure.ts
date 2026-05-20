import type { BackendId } from "../types.js";

const AUTH_FAILURE_PATTERNS: ReadonlyArray<RegExp> = [
  /401\s+unauthorized/i,
  /failed to refresh token/i,
  /not\s+(logged in|authenticated|signed in)/i,
  /please (log|sign) in/i,
  /authentication (failed|error|required)/i,
  /invalid (api[_ ]?key|credentials)/i,
  /no\s+(valid\s+)?(credentials|api key)/i,
  /token\s+(has\s+)?expired/i,
];

export const isAuthFailure = (text: string): boolean => {
  if (!text) return false;
  return AUTH_FAILURE_PATTERNS.some((pattern) => pattern.test(text));
};

export const formatAuthFailureMessage = (
  backend: BackendId,
  loginCommand: string,
  launchMessage?: string,
): string => {
  const lines = [
    `[${backend}] authentication appears to have failed during execution.`,
    `Run \`${loginCommand}\` to re-authenticate.`,
  ];
  if (launchMessage) {
    lines.push("", launchMessage);
  }
  return lines.join("\n");
};
