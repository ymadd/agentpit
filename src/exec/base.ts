import { spawn } from "node:child_process";

import type { BackendId } from "../types.js";

export interface ExecSpec {
  readonly command: string;
  readonly args: ReadonlyArray<string>;
  readonly env?: Readonly<Record<string, string | undefined>>;
  readonly stdinInput?: string;
}

export interface ExecResult {
  readonly output: string;
  readonly exitCode: number | null;
  readonly signal: NodeJS.Signals | null;
}

export interface ExecRunOptions {
  readonly cwd: string;
  readonly signal?: AbortSignal;
  readonly onStdout?: (chunk: string) => void;
}

export interface ExecAdapter {
  readonly id: BackendId;
  buildSpec(task: string): ExecSpec;
  run(task: string, options: ExecRunOptions): Promise<ExecResult>;
}

export abstract class BaseExecAdapter implements ExecAdapter {
  abstract readonly id: BackendId;
  abstract buildSpec(task: string): ExecSpec;

  async run(task: string, options: ExecRunOptions): Promise<ExecResult> {
    const spec = this.buildSpec(task);
    const env = {
      ...process.env,
      ...spec.env,
    };

    const child = spawn(spec.command, [...spec.args], {
      cwd: options.cwd,
      env,
      stdio: ["pipe", "pipe", "pipe"],
    });

    if (options.signal) {
      const abortHandler = (): void => {
        if (!child.killed) child.kill("SIGTERM");
      };
      options.signal.addEventListener("abort", abortHandler, { once: true });
    }

    const collected: string[] = [];
    const stderr: string[] = [];

    child.stdout?.setEncoding("utf8");
    child.stderr?.setEncoding("utf8");

    child.stdout?.on("data", (chunk: string) => {
      collected.push(chunk);
      options.onStdout?.(chunk);
    });
    child.stderr?.on("data", (chunk: string) => {
      stderr.push(chunk);
    });

    if (spec.stdinInput !== undefined && child.stdin) {
      child.stdin.write(spec.stdinInput);
      child.stdin.end();
    } else {
      child.stdin?.end();
    }

    return new Promise<ExecResult>((resolve, reject) => {
      child.once("error", reject);
      child.once("close", (code, signalName) => {
        if (code !== 0 && code !== null) {
          const stderrText = stderr.join("").trim();
          const detail = stderrText.length > 0 ? `\nstderr: ${stderrText}` : "";
          reject(
            new Error(
              `${this.id} exited with code ${code}${detail}`,
            ),
          );
          return;
        }
        resolve({
          output: collected.join(""),
          exitCode: code,
          signal: signalName,
        });
      });
    });
  }
}
