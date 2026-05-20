import { access } from "node:fs/promises";
import { constants } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";
import { spawn } from "node:child_process";

import type { BackendId } from "../types.js";

export interface AuthStatus {
  readonly ok: boolean;
  readonly backend: BackendId;
  readonly hint: string;
  readonly loginCommand: string;
}

const fileExists = async (path: string): Promise<boolean> => {
  try {
    await access(path, constants.R_OK);
    return true;
  } catch {
    return false;
  }
};

const runWithExitCode = (
  command: string,
  args: ReadonlyArray<string>,
): Promise<number> =>
  new Promise((resolve) => {
    const child = spawn(command, [...args], { stdio: "ignore" });
    child.once("error", () => resolve(127));
    child.once("close", (code) => resolve(code ?? 1));
  });

const checkCodex = async (): Promise<AuthStatus> => {
  const exitCode = await runWithExitCode("codex", ["login", "status"]);
  const ok = exitCode === 0;
  return {
    ok,
    backend: "codex",
    hint: ok
      ? "Codex is authenticated."
      : "Codex CLI is not logged in. Authenticate it once via OAuth.",
    loginCommand: "codex login",
  };
};

const checkGemini = async (): Promise<AuthStatus> => {
  const credsPath = join(homedir(), ".gemini", "oauth_creds.json");
  const ok = await fileExists(credsPath);
  return {
    ok,
    backend: "gemini",
    hint: ok
      ? "Gemini OAuth credentials are present."
      : "Gemini CLI has no OAuth credentials. Launch it once to log in.",
    loginCommand: "gemini",
  };
};

const checkClaude = async (): Promise<AuthStatus> => {
  const configPath = join(homedir(), ".claude.json");
  const ok = await fileExists(configPath);
  return {
    ok,
    backend: "claude",
    hint: ok
      ? "Claude Code config is present."
      : "Claude Code is not configured. Run it once interactively to sign in.",
    loginCommand: "claude",
  };
};

const checkOpencode = async (): Promise<AuthStatus> => {
  const binaryPath = join(homedir(), ".opencode", "bin", "opencode");
  const binaryOk = await fileExists(binaryPath);
  if (!binaryOk) {
    return {
      ok: false,
      backend: "opencode",
      hint: "OpenCode binary not found at ~/.opencode/bin/opencode.",
      loginCommand: "curl -fsSL https://opencode.ai/install | bash",
    };
  }
  return {
    ok: true,
    backend: "opencode",
    hint: "OpenCode binary present. Free models work out of the box; run `opencode auth login` only if you want to add paid providers.",
    loginCommand: `${binaryPath} auth login`,
  };
};

const checkers: Partial<Record<BackendId, () => Promise<AuthStatus>>> = {
  codex: checkCodex,
  gemini: checkGemini,
  claude: checkClaude,
  opencode: checkOpencode,
};

export const checkAuth = async (backend: BackendId): Promise<AuthStatus> => {
  const checker = checkers[backend];
  if (!checker) {
    return {
      ok: false,
      backend,
      hint: `No auth checker registered for backend ${backend}.`,
      loginCommand: "",
    };
  }
  return checker();
};
