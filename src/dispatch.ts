import type { ExecAdapter } from "./exec/base.js";
import type { BaseAdapter } from "./adapters/base.js";
import type { BackendId } from "./types.js";

export type ExecRegistry = Readonly<Partial<Record<BackendId, ExecAdapter>>>;
export type AcpRegistry = Readonly<Partial<Record<BackendId, BaseAdapter>>>;

export type Transport = "exec" | "acp";

export interface DispatchRegistries {
  readonly execs: ExecRegistry;
  readonly acps: AcpRegistry;
}

export interface DispatchResult {
  readonly backend: BackendId;
  readonly transport: Transport;
  readonly output: string;
}

export const resolveTransport = (
  backendId: BackendId,
  deps: DispatchRegistries,
): Transport | null => {
  if (deps.execs[backendId]) return "exec";
  if (deps.acps[backendId]) return "acp";
  return null;
};

export const dispatchOnBackend = async (
  backendId: BackendId,
  task: string,
  cwd: string,
  signal: AbortSignal,
  onChunk: (chunk: string) => void,
  deps: DispatchRegistries,
): Promise<DispatchResult> => {
  const exec = deps.execs[backendId];
  if (exec) {
    const result = await exec.run(task, {
      cwd,
      signal,
      onStdout: onChunk,
    });
    return { backend: backendId, transport: "exec", output: result.output };
  }
  const acp = deps.acps[backendId];
  if (acp) {
    const sessionId = await acp.newSession(cwd);
    const cancelOnAbort = (): void => {
      acp.cancel(sessionId).catch(() => {
        /* best-effort cancel */
      });
    };
    signal.addEventListener("abort", cancelOnAbort, { once: true });

    const buffer: string[] = [];
    try {
      await acp.prompt(sessionId, task, (chunk) => {
        buffer.push(chunk);
        onChunk(chunk);
      });
      return {
        backend: backendId,
        transport: "acp",
        output: buffer.join(""),
      };
    } finally {
      signal.removeEventListener("abort", cancelOnAbort);
    }
  }
  throw new Error(`No transport registered for backend ${backendId}`);
};
