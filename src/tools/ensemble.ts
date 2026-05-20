import { z } from "zod";
import type { RequestHandlerExtra } from "@modelcontextprotocol/sdk/shared/protocol.js";
import type {
  ServerRequest,
  ServerNotification,
} from "@modelcontextprotocol/sdk/types.js";

import { BackendIdSchema, type BackendId } from "../types.js";
import { checkAuth } from "../auth/check.js";
import { isAuthFailure } from "../auth/detect-failure.js";
import {
  dispatchOnBackend,
  resolveTransport,
  type AcpRegistry,
  type ExecRegistry,
  type Transport,
} from "../dispatch.js";
import type { HubConfig } from "../config.js";

export const ensembleInputShape = {
  prompt: z.string().min(1).describe("Prompt to fan out to all members."),
  members: z
    .array(BackendIdSchema)
    .min(1)
    .optional()
    .describe(
      "Backends to run in parallel. Defaults to config.ensemble.default_members.",
    ),
  aggregator: BackendIdSchema.optional().describe(
    "If set, run this backend on the combined outputs to produce a single synthesis.",
  ),
  cwd: z.string().optional(),
};

export type EnsembleArgs = {
  prompt: string;
  members?: BackendId[];
  aggregator?: BackendId;
  cwd?: string;
};

export interface EnsembleDeps {
  readonly execs: ExecRegistry;
  readonly acps: AcpRegistry;
  readonly config: HubConfig;
  readonly defaultCwd: string;
}

type ToolResult = {
  content: Array<{ type: "text"; text: string }>;
  isError?: boolean;
};

export interface MemberOutcome {
  readonly backend: BackendId;
  readonly transport: Transport | "skipped";
  readonly output?: string;
  readonly error?: string;
}

export const buildAggregatorPrompt = (
  originalPrompt: string,
  outcomes: ReadonlyArray<MemberOutcome>,
): string => {
  const lines = [
    "You are aggregating independent responses from multiple coding agents to the user's original task.",
    "Synthesize one best answer. Note disagreements explicitly. Cite each source as [backend].",
    "",
    `# Original task`,
    originalPrompt,
    "",
    `# Responses`,
  ];
  for (const o of outcomes) {
    if (o.output) {
      lines.push("", `## [${o.backend}]`, o.output.trim());
    } else if (o.error) {
      lines.push("", `## [${o.backend}] (failed)`, o.error);
    }
  }
  return lines.join("\n");
};

export const renderConcatenatedOutput = (
  outcomes: ReadonlyArray<MemberOutcome>,
): string => {
  const sections: string[] = [];
  for (const o of outcomes) {
    const header = `=== ${o.backend} (transport=${o.transport}) ===`;
    const body =
      o.output?.trim() ??
      (o.error ? `[error] ${o.error}` : "(no output)");
    sections.push(`${header}\n${body}`);
  }
  return sections.join("\n\n");
};

const runOneMember = async (
  backendId: BackendId,
  prompt: string,
  cwd: string,
  signal: AbortSignal,
  onChunk: (source: BackendId, chunk: string) => void,
  deps: EnsembleDeps,
): Promise<MemberOutcome> => {
  const transport = resolveTransport(backendId, deps);
  if (!transport) {
    return {
      backend: backendId,
      transport: "skipped",
      error: `not registered`,
    };
  }
  const auth = await checkAuth(backendId);
  if (!auth.ok) {
    return {
      backend: backendId,
      transport: "skipped",
      error: `auth missing — ${auth.hint}`,
    };
  }
  try {
    const result = await dispatchOnBackend(
      backendId,
      prompt,
      cwd,
      signal,
      (chunk) => onChunk(backendId, chunk),
      deps,
    );
    if (isAuthFailure(result.output)) {
      return {
        backend: backendId,
        transport: result.transport,
        error: "auth failure during execution",
      };
    }
    return {
      backend: backendId,
      transport: result.transport,
      output: result.output,
    };
  } catch (error) {
    return {
      backend: backendId,
      transport,
      error: error instanceof Error ? error.message : String(error),
    };
  }
};

export const handleEnsemble = async (
  args: EnsembleArgs,
  extra: RequestHandlerExtra<ServerRequest, ServerNotification>,
  deps: EnsembleDeps,
): Promise<ToolResult> => {
  const members = args.members ?? deps.config.ensemble.default_members;
  const aggregator = args.aggregator ?? deps.config.ensemble.aggregator;
  const cwd = args.cwd ?? deps.defaultCwd;
  const progressToken = extra._meta?.progressToken;

  let chunkIndex = 0;
  const sendProgress = (source: BackendId | "aggregator", chunk: string): void => {
    if (progressToken === undefined) return;
    extra
      .sendNotification({
        method: "notifications/progress",
        params: {
          progressToken,
          progress: ++chunkIndex,
          _meta: { source, text: chunk },
        },
      })
      .catch(() => {
        /* progress is best-effort */
      });
  };

  // Run members in parallel
  const outcomes = await Promise.all(
    members.map((id) =>
      runOneMember(id, args.prompt, cwd, extra.signal, sendProgress, deps),
    ),
  );

  const successes = outcomes.filter((o) => o.output !== undefined);
  const memberSection = renderConcatenatedOutput(outcomes);

  // Optionally synthesize via aggregator
  if (aggregator && successes.length > 0) {
    const aggTransport = resolveTransport(aggregator, deps);
    if (!aggTransport) {
      return {
        content: [
          {
            type: "text",
            text: `${memberSection}\n\n=== aggregator skipped ===\n${aggregator} not registered`,
          },
        ],
      };
    }
    const aggAuth = await checkAuth(aggregator);
    if (!aggAuth.ok) {
      return {
        content: [
          {
            type: "text",
            text: `${memberSection}\n\n=== aggregator skipped ===\nauth missing for ${aggregator}: ${aggAuth.hint}`,
          },
        ],
      };
    }
    const aggPrompt = buildAggregatorPrompt(args.prompt, outcomes);
    try {
      const aggResult = await dispatchOnBackend(
        aggregator,
        aggPrompt,
        cwd,
        extra.signal,
        (chunk) => sendProgress("aggregator", chunk),
        deps,
      );
      return {
        content: [
          {
            type: "text",
            text: `${memberSection}\n\n=== aggregator [${aggregator}] (transport=${aggResult.transport}) ===\n${aggResult.output.trim()}`,
          },
        ],
      };
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      return {
        content: [
          {
            type: "text",
            text: `${memberSection}\n\n=== aggregator failed ===\n${aggregator}: ${message}`,
          },
        ],
        isError: true,
      };
    }
  }

  return {
    content: [{ type: "text", text: memberSection }],
    isError: successes.length === 0,
  };
};
