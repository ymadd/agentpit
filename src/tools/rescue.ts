import { z } from "zod";
import type { RequestHandlerExtra } from "@modelcontextprotocol/sdk/shared/protocol.js";
import type {
  ServerRequest,
  ServerNotification,
} from "@modelcontextprotocol/sdk/types.js";

import { BackendIdSchema, type BackendId } from "../types.js";
import { checkAuth } from "../auth/check.js";
import { launchLogin } from "../auth/launch.js";
import {
  formatAuthFailureMessage,
  isAuthFailure,
} from "../auth/detect-failure.js";
import type { Router } from "../router.js";
import type { RouteKey } from "../config.js";
import {
  dispatchOnBackend,
  type AcpRegistry,
  type ExecRegistry,
} from "../dispatch.js";

export { type ExecRegistry, type AcpRegistry } from "../dispatch.js";

export const rescueInputShape = {
  task: z.string().min(1).describe("Task description to delegate"),
  backend: BackendIdSchema.optional().describe("Override target backend id"),
  cwd: z
    .string()
    .optional()
    .describe("Working directory (absolute). Defaults to the hub's cwd."),
  auto_login: z
    .boolean()
    .optional()
    .describe(
      "If true (default), open the backend's login flow in a new Terminal window when not authenticated.",
    ),
};

export type RescueArgs = {
  task: string;
  backend?: BackendId;
  cwd?: string;
  auto_login?: boolean;
};

export interface RescueDeps {
  readonly execs: ExecRegistry;
  readonly acps: AcpRegistry;
  readonly router: Router;
  readonly defaultCwd: string;
  readonly routeKey: RouteKey;
}

type ToolResult = {
  content: Array<{ type: "text"; text: string }>;
  isError?: boolean;
};

const errorResult = (text: string): ToolResult => ({
  content: [{ type: "text", text }],
  isError: true,
});

export const handleRescue = async (
  args: RescueArgs,
  extra: RequestHandlerExtra<ServerRequest, ServerNotification>,
  deps: RescueDeps,
): Promise<ToolResult> => {
  const decision = deps.router.resolve({
    tool: deps.routeKey,
    explicitBackend: args.backend,
    task: args.task,
  });
  const backendId = decision.backend;

  if (!deps.execs[backendId] && !deps.acps[backendId]) {
    const available = Array.from(
      new Set([...Object.keys(deps.execs), ...Object.keys(deps.acps)]),
    ).join(", ");
    return errorResult(
      `Unsupported backend resolved: ${backendId} (route: ${decision.reason}). Available: ${available}`,
    );
  }

  const autoLogin = args.auto_login ?? true;
  const authStatus = await checkAuth(backendId);
  if (!authStatus.ok) {
    if (autoLogin) {
      const { launchResult } = await launchLogin(backendId);
      const lines = [
        `[${backendId}] is not authenticated.`,
        authStatus.hint,
        `Login command: ${authStatus.loginCommand}`,
      ];
      if (launchResult) {
        lines.push("", launchResult.message);
        if (launchResult.launched) {
          lines.push(
            "Complete the OAuth flow in the opened Terminal window, then re-run the tool.",
          );
        }
      }
      return errorResult(lines.join("\n"));
    }
    return errorResult(
      `[${backendId}] not authenticated. Run \`${authStatus.loginCommand}\`, or call the login tool.`,
    );
  }

  const cwd = args.cwd ?? deps.defaultCwd;
  const progressToken = extra._meta?.progressToken;

  let chunkIndex = 0;
  const sendProgress = async (text: string): Promise<void> => {
    if (progressToken === undefined) return;
    try {
      await extra.sendNotification({
        method: "notifications/progress",
        params: {
          progressToken,
          progress: ++chunkIndex,
          _meta: { source: backendId, text },
        },
      });
    } catch {
      /* progress is best-effort */
    }
  };

  try {
    const result = await dispatchOnBackend(
      backendId,
      args.task,
      cwd,
      extra.signal,
      (chunk) => {
        void sendProgress(chunk);
      },
      { execs: deps.execs, acps: deps.acps },
    );

    if (isAuthFailure(result.output)) {
      let launchMessage: string | undefined;
      if (autoLogin) {
        const { launchResult } = await launchLogin(backendId);
        launchMessage = launchResult?.message;
      }
      return errorResult(
        formatAuthFailureMessage(
          backendId,
          authStatus.loginCommand,
          launchMessage,
        ),
      );
    }

    const trimmed = result.output.trim();
    const header = `[backend=${backendId} transport=${result.transport} route=${decision.reason}]\n`;
    return {
      content: [
        {
          type: "text",
          text: trimmed.length > 0 ? header + trimmed : `${header}(no output)`,
        },
      ],
    };
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    if (isAuthFailure(message)) {
      let launchMessage: string | undefined;
      if (autoLogin) {
        const { launchResult } = await launchLogin(backendId);
        launchMessage = launchResult?.message;
      }
      return errorResult(
        formatAuthFailureMessage(
          backendId,
          authStatus.loginCommand,
          launchMessage,
        ),
      );
    }
    return errorResult(`rescue failed (backend=${backendId}): ${message}`);
  }
};
