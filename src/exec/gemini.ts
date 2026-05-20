import { BaseExecAdapter, type ExecSpec } from "./base.js";
import type { BackendId } from "../types.js";

export class GeminiExec extends BaseExecAdapter {
  readonly id: BackendId = "gemini";

  buildSpec(task: string): ExecSpec {
    return {
      command: "gemini",
      args: [
        "--yolo",
        "--skip-trust",
        "--output-format",
        "text",
        "-p",
        task,
      ],
    };
  }
}
