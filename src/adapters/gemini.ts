import { BaseAdapter, type SpawnSpec } from "./base.js";
import type { BackendId } from "../types.js";

export class GeminiAdapter extends BaseAdapter {
  readonly id: BackendId = "gemini";

  protected spawnSpec(): SpawnSpec {
    return {
      command: "gemini",
      args: ["--acp"],
    };
  }
}
