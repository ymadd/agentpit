import { spawn } from "node:child_process";
import { platform } from "node:os";

import type { BackendId } from "../types.js";
import { checkAuth, type AuthStatus } from "./check.js";

export interface LaunchResult {
  readonly launched: boolean;
  readonly message: string;
}

const escapeForAppleScript = (input: string): string =>
  input.replace(/\\/g, "\\\\").replace(/"/g, '\\"');

const launchInMacTerminal = (command: string): Promise<LaunchResult> =>
  new Promise((resolve) => {
    const script = `tell application "Terminal" to do script "${escapeForAppleScript(command)}"`;
    const child = spawn("osascript", ["-e", script], { stdio: "ignore" });
    child.once("error", (error) => {
      resolve({
        launched: false,
        message: `Failed to open Terminal.app: ${error.message}`,
      });
    });
    child.once("close", (code) => {
      if (code === 0) {
        resolve({
          launched: true,
          message: `Opened Terminal.app and ran: ${command}`,
        });
      } else {
        resolve({
          launched: false,
          message: `osascript exited with code ${code}`,
        });
      }
    });
  });

export const launchLogin = async (
  backend: BackendId,
): Promise<{
  status: AuthStatus;
  launchResult?: LaunchResult;
}> => {
  const status = await checkAuth(backend);
  if (status.ok) {
    return { status };
  }
  if (!status.loginCommand) {
    return { status };
  }
  if (platform() === "darwin") {
    const launchResult = await launchInMacTerminal(status.loginCommand);
    return { status, launchResult };
  }
  return {
    status,
    launchResult: {
      launched: false,
      message: `Auto-launch is only supported on macOS. Run manually: ${status.loginCommand}`,
    },
  };
};
