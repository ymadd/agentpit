import { BaseAdapter, type SpawnSpec } from "./base.js";
import type { BackendId } from "../types.js";

export class CodexAdapter extends BaseAdapter {
  readonly id: BackendId = "codex";

  protected spawnSpec(): SpawnSpec {
    return {
      command: "npx",
      args: ["-y", "@zed-industries/codex-acp@0.14.0"],
      env: {
        OPENAI_API_KEY: process.env.OPENAI_API_KEY,
        CODEX_API_KEY: process.env.CODEX_API_KEY,
      },
    };
  }
}
