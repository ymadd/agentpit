import { z } from "zod";
import type { RequestHandlerExtra } from "@modelcontextprotocol/sdk/shared/protocol.js";
import type {
  ServerRequest,
  ServerNotification,
} from "@modelcontextprotocol/sdk/types.js";

import { BackendIdSchema } from "../types.js";
import { handleRescue, type RescueDeps } from "./rescue.js";

export const explainInputShape = {
  target: z
    .string()
    .min(1)
    .describe("Target to explain: a path, glob, function name, or topic."),
  depth: z
    .enum(["brief", "deep"])
    .optional()
    .describe("Explanation depth (default: brief)."),
  backend: BackendIdSchema.optional(),
  cwd: z.string().optional(),
};

export type ExplainArgs = {
  target: string;
  depth?: "brief" | "deep";
  backend?: z.infer<typeof BackendIdSchema>;
  cwd?: string;
};

export const handleExplain = async (
  args: ExplainArgs,
  extra: RequestHandlerExtra<ServerRequest, ServerNotification>,
  deps: Omit<RescueDeps, "routeKey">,
): ReturnType<typeof handleRescue> => {
  const depth = args.depth ?? "brief";
  const lines = [
    `Explain: ${args.target}`,
    depth === "brief"
      ? "Keep the explanation tight (under 200 words). Lead with purpose, then mechanism."
      : "Provide a deep walk-through: design rationale, control flow, edge cases, and how it interacts with the surrounding system.",
  ];
  return handleRescue(
    {
      task: lines.join("\n"),
      backend: args.backend,
      cwd: args.cwd,
    },
    extra,
    { ...deps, routeKey: "explain" },
  );
};
