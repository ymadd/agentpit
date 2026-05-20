import { McpServer } from "@modelcontextprotocol/sdk/server/mcp.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";

import { GeminiExec } from "./exec/gemini.js";
import { ClaudeExec } from "./exec/claude.js";
import { CodexExec } from "./exec/codex.js";
import { OpencodeAdapter } from "./adapters/opencode.js";
import { ClaudeAdapter } from "./adapters/claude.js";
import { GeminiAdapter } from "./adapters/gemini.js";
import { CodexAdapter } from "./adapters/codex.js";
import type { BaseAdapter } from "./adapters/base.js";
import {
  handleRescue,
  rescueInputShape,
  type ExecRegistry,
  type AcpRegistry,
} from "./tools/rescue.js";
import { handleReview, reviewInputShape } from "./tools/review.js";
import { handleExplain, explainInputShape } from "./tools/explain.js";
import { handleRefactor, refactorInputShape } from "./tools/refactor.js";
import { handleEnsemble, ensembleInputShape } from "./tools/ensemble.js";
import { handleStatus, statusInputShape } from "./tools/status.js";
import { handleLogin, loginInputShape } from "./tools/login.js";
import { loadConfig } from "./config.js";
import { Router } from "./router.js";
import type { BackendId } from "./types.js";

type Transport = "exec" | "acp";

const DEFAULT_TRANSPORTS: Partial<Record<BackendId, Transport>> = {
  gemini: "exec",
  claude: "exec",
  codex: "exec",
  opencode: "acp",
};

const buildRegistries = (
  overrides: Partial<Record<BackendId, { transport?: Transport }>>,
): { execs: ExecRegistry; acps: AcpRegistry } => {
  const execs: Partial<Record<BackendId, ExecRegistry[BackendId]>> = {};
  const acps: Partial<Record<BackendId, AcpRegistry[BackendId]>> = {};

  const transportFor = (id: BackendId): Transport | undefined =>
    overrides[id]?.transport ?? DEFAULT_TRANSPORTS[id];

  if (transportFor("gemini") === "exec") execs.gemini = new GeminiExec();
  else if (transportFor("gemini") === "acp") acps.gemini = new GeminiAdapter();

  if (transportFor("claude") === "exec") execs.claude = new ClaudeExec();
  else if (transportFor("claude") === "acp") acps.claude = new ClaudeAdapter();

  if (transportFor("codex") === "exec") execs.codex = new CodexExec();
  else if (transportFor("codex") === "acp") acps.codex = new CodexAdapter();

  // opencode is acp-only for now
  if (transportFor("opencode") === "acp") acps.opencode = new OpencodeAdapter();

  return { execs, acps };
};

export const startMcpServer = async (): Promise<void> => {
  const { config, source: configSource, path: configPath } = await loadConfig();
  const { execs, acps } = buildRegistries(config.backends);
  const available = new Set<BackendId>([
    ...(Object.keys(execs) as BackendId[]),
    ...(Object.keys(acps) as BackendId[]),
  ]);
  const router = new Router(config, available);
  const defaultCwd = process.cwd();

  const baseDeps = { execs, acps, router, defaultCwd };
  const ensembleDeps = { execs, acps, config, defaultCwd };

  const server = new McpServer({
    name: "agentpit",
    version: "0.1.0",
  });

  server.registerTool(
    "rescue",
    {
      title: "Delegate a one-shot task to a backend coding agent",
      description:
        "Route a one-shot task to a chosen backend. Uses CLI direct (exec) for gemini/claude and ACP for opencode. Streams via notifications/progress.",
      inputSchema: rescueInputShape,
    },
    (args, extra) =>
      handleRescue(args, extra, { ...baseDeps, routeKey: "rescue" }),
  );

  server.registerTool(
    "review",
    {
      title: "Multi-agent code review (gemini + opencode by default)",
      description:
        "Run a code review on the given target across multiple backends in parallel. Defaults to gemini + opencode; configurable via config.ensemble.review_members or the `members` argument.",
      inputSchema: reviewInputShape,
    },
    (args, extra) => handleReview(args, extra, ensembleDeps),
  );

  server.registerTool(
    "explain",
    {
      title: "Explain code via backend agent",
      description:
        "Explain a target (file, function, topic). Default route prefers Gemini for long context.",
      inputSchema: explainInputShape,
    },
    (args, extra) => handleExplain(args, extra, baseDeps),
  );

  server.registerTool(
    "refactor",
    {
      title: "Plan a refactor via backend agent",
      description:
        "Propose a refactor plan + diff. Default route prefers Claude.",
      inputSchema: refactorInputShape,
    },
    (args, extra) => handleRefactor(args, extra, baseDeps),
  );

  server.registerTool(
    "ensemble",
    {
      title: "Run a prompt across multiple backends in parallel",
      description:
        "Fan out the same prompt to N backends. Optionally synthesize via an aggregator backend. Defaults: members=gemini/claude/opencode, no aggregator.",
      inputSchema: ensembleInputShape,
    },
    (args, extra) => handleEnsemble(args, extra, ensembleDeps),
  );

  server.registerTool(
    "status",
    {
      title: "agentpit status report",
      description:
        "Show config source, default backend, and per-backend registration / auth state.",
      inputSchema: statusInputShape,
    },
    (args) =>
      handleStatus(args, { execs, acps, config, configPath, configSource }),
  );

  server.registerTool(
    "login",
    {
      title: "Authenticate a backend coding agent",
      description:
        "Check a backend's auth status, optionally opening its login flow in a new Terminal window.",
      inputSchema: loginInputShape,
    },
    (args) => handleLogin(args),
  );

  const acpAdapters: ReadonlyArray<BaseAdapter> = Object.values(acps).filter(
    (a): a is BaseAdapter => a !== undefined,
  );

  const cleanup = async (): Promise<void> => {
    await Promise.allSettled(acpAdapters.map((a) => a.close()));
  };

  for (const signal of ["SIGINT", "SIGTERM"] as const) {
    process.once(signal, () => {
      cleanup().finally(() => process.exit(0));
    });
  }

  const transport = new StdioServerTransport();
  await server.connect(transport);
};
