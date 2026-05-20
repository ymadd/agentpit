import { z } from "zod";

import { BackendIdSchema, type BackendId } from "../types.js";
import { checkAuth } from "../auth/check.js";
import { launchLogin } from "../auth/launch.js";

export const loginInputShape = {
  backend: BackendIdSchema.describe(
    "Backend to authenticate (codex|gemini|claude)",
  ),
  check_only: z
    .boolean()
    .optional()
    .describe("If true, only report status without launching the login flow."),
};

export type LoginArgs = {
  backend: BackendId;
  check_only?: boolean;
};

export const handleLogin = async (
  args: LoginArgs,
): Promise<{ content: Array<{ type: "text"; text: string }> }> => {
  if (args.check_only) {
    const status = await checkAuth(args.backend);
    return {
      content: [
        {
          type: "text",
          text: status.ok
            ? `[${status.backend}] authenticated`
            : `[${status.backend}] NOT authenticated.\n${status.hint}\nLogin command: ${status.loginCommand}`,
        },
      ],
    };
  }

  const { status, launchResult } = await launchLogin(args.backend);
  if (status.ok) {
    return {
      content: [
        {
          type: "text",
          text: `[${status.backend}] already authenticated.`,
        },
      ],
    };
  }

  const lines = [
    `[${status.backend}] is not authenticated.`,
    status.hint,
    `Login command: ${status.loginCommand}`,
  ];
  if (launchResult) {
    lines.push("");
    lines.push(launchResult.message);
    if (launchResult.launched) {
      lines.push(
        "Complete the OAuth flow in the new Terminal window, then retry the previous tool call.",
      );
    }
  }
  return { content: [{ type: "text", text: lines.join("\n") }] };
};
