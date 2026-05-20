import { BaseExecAdapter, type ExecSpec } from "./base.js";
import type { BackendId } from "../types.js";

export class CodexExec extends BaseExecAdapter {
  readonly id: BackendId = "codex";

  buildSpec(task: string): ExecSpec {
    return {
      command: "codex",
      args: ["exec", "--skip-git-repo-check", "-"],
      stdinInput: task,
    };
  }
}
