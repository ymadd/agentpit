import { homedir } from "node:os";
import { join } from "node:path";

import { BaseAdapter, type SpawnSpec } from "./base.js";
import type { BackendId } from "../types.js";

const candidateCommands = (): ReadonlyArray<string> => [
  "opencode",
  join(homedir(), ".opencode", "bin", "opencode"),
];

export const resolveOpencodeCommand = (): string => {
  // Prefer PATH lookup; fall back to the standard ~/.opencode install path.
  // We don't synchronously stat each candidate — the BaseAdapter will fail loudly
  // if the chosen command isn't executable, which gives a clearer error message.
  return candidateCommands()[1];
};

export class OpencodeAdapter extends BaseAdapter {
  readonly id: BackendId = "opencode";

  protected spawnSpec(): SpawnSpec {
    return {
      command: resolveOpencodeCommand(),
      args: ["acp"],
    };
  }
}
