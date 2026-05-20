import { BackendIdSchema, type BackendId } from "../types.js";
import { checkAuth } from "../auth/check.js";
import type { ExecRegistry, AcpRegistry } from "./rescue.js";
import type { HubConfig } from "../config.js";

export const statusInputShape = {
  backend: BackendIdSchema.optional().describe(
    "Limit the status report to a single backend.",
  ),
};

export type StatusArgs = {
  backend?: BackendId;
};

export interface StatusDeps {
  readonly execs: ExecRegistry;
  readonly acps: AcpRegistry;
  readonly config: HubConfig;
  readonly configPath: string;
  readonly configSource: "file" | "defaults";
}

const inspectBackend = async (
  backendId: BackendId,
  deps: StatusDeps,
): Promise<string> => {
  const inExec = backendId in deps.execs;
  const inAcp = backendId in deps.acps;
  if (!inExec && !inAcp) {
    return `[${backendId}] not registered`;
  }
  const transport = inExec ? "exec" : "acp";
  const auth = await checkAuth(backendId);
  const authLine = auth.ok ? "auth=ok" : `auth=missing (${auth.loginCommand})`;
  return `[${backendId}] transport=${transport} ${authLine}`;
};

export const handleStatus = async (
  args: StatusArgs,
  deps: StatusDeps,
): Promise<{ content: Array<{ type: "text"; text: string }> }> => {
  const allBackends = Array.from(
    new Set<BackendId>([
      ...(Object.keys(deps.execs) as BackendId[]),
      ...(Object.keys(deps.acps) as BackendId[]),
    ]),
  );
  const targets: ReadonlyArray<BackendId> = args.backend
    ? [args.backend]
    : allBackends;

  const lines: string[] = [
    `config: ${deps.configSource} (${deps.configPath})`,
    `default backend: ${deps.config.default.backend}`,
    `auto_route: ${deps.config.default.auto_route ? "on" : "off"}`,
    "",
    "backends:",
  ];
  for (const id of targets) {
    lines.push(`  ${await inspectBackend(id, deps)}`);
  }
  return { content: [{ type: "text", text: lines.join("\n") }] };
};
