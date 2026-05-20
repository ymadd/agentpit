import { z } from "zod";
import type { RequestHandlerExtra } from "@modelcontextprotocol/sdk/shared/protocol.js";
import type {
  ServerRequest,
  ServerNotification,
} from "@modelcontextprotocol/sdk/types.js";

import { BackendIdSchema } from "../types.js";
import { handleRescue, type RescueDeps } from "./rescue.js";

export const refactorInputShape = {
  path: z.string().min(1).describe("File or directory to refactor."),
  goal: z.string().min(1).describe("Concrete refactoring goal."),
  backend: BackendIdSchema.optional(),
  cwd: z.string().optional(),
};

export type RefactorArgs = {
  path: string;
  goal: string;
  backend?: z.infer<typeof BackendIdSchema>;
  cwd?: string;
};

export const handleRefactor = async (
  args: RefactorArgs,
  extra: RequestHandlerExtra<ServerRequest, ServerNotification>,
  deps: Omit<RescueDeps, "routeKey">,
): ReturnType<typeof handleRescue> => {
  const lines = [
    `Refactor target: ${args.path}`,
    `Goal: ${args.goal}`,
    "Plan the change first (what changes, why, in what order).",
    "Then propose the concrete edits as a unified diff if possible.",
    "Do not apply destructive operations without explicit user approval.",
  ];
  return handleRescue(
    {
      task: lines.join("\n"),
      backend: args.backend,
      cwd: args.cwd,
    },
    extra,
    { ...deps, routeKey: "refactor" },
  );
};
