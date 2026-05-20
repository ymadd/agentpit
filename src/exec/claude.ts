import { BaseExecAdapter, type ExecSpec } from "./base.js";
import type { BackendId } from "../types.js";

export class ClaudeExec extends BaseExecAdapter {
  readonly id: BackendId = "claude";

  buildSpec(task: string): ExecSpec {
    return {
      command: "claude",
      args: [
        "--print",
        "--output-format",
        "text",
        "--permission-mode",
        "acceptEdits",
        task,
      ],
    };
  }
}
