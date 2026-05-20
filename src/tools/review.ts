import { z } from "zod";
import type { RequestHandlerExtra } from "@modelcontextprotocol/sdk/shared/protocol.js";
import type {
  ServerRequest,
  ServerNotification,
} from "@modelcontextprotocol/sdk/types.js";

import { BackendIdSchema, type BackendId } from "../types.js";
import { handleEnsemble, type EnsembleDeps } from "./ensemble.js";

export const reviewInputShape = {
  target: z
    .string()
    .min(1)
    .describe(
      "Target to review: a glob (src/**/*.ts), a path, or a free-form description.",
    ),
  focus: z
    .string()
    .optional()
    .describe(
      "Reviewer focus (e.g. 'security', 'concurrency', 'naming'). Optional.",
    ),
  members: z
    .array(BackendIdSchema)
    .optional()
    .describe(
      "Override the reviewer panel. Defaults to config.ensemble.review_members.",
    ),
  aggregator: BackendIdSchema.optional().describe(
    "Optional synthesizer for the panel.",
  ),
  cwd: z.string().optional(),
};

export type ReviewArgs = {
  target: string;
  focus?: string;
  members?: BackendId[];
  aggregator?: BackendId;
  cwd?: string;
};

export const handleReview = async (
  args: ReviewArgs,
  extra: RequestHandlerExtra<ServerRequest, ServerNotification>,
  deps: EnsembleDeps,
): ReturnType<typeof handleEnsemble> => {
  const lines = [
    `Perform a thorough code review of: ${args.target}`,
    "Report concrete issues with file:line citations.",
    "Categorize each finding as CRITICAL / HIGH / MEDIUM / LOW.",
    "If you cannot access files, say so explicitly.",
  ];
  if (args.focus) lines.push(`Reviewer focus: ${args.focus}.`);

  const members = args.members ?? deps.config.ensemble.review_members;
  const aggregator = args.aggregator ?? deps.config.ensemble.review_aggregator;

  return handleEnsemble(
    {
      prompt: lines.join("\n"),
      members,
      aggregator,
      cwd: args.cwd,
    },
    extra,
    deps,
  );
};
