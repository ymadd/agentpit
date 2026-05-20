import { BaseAdapter, type SpawnSpec } from "./base.js";
import type { BackendId } from "../types.js";

export class ClaudeAdapter extends BaseAdapter {
  readonly id: BackendId = "claude";

  protected spawnSpec(): SpawnSpec {
    // NOTE: @agentclientprotocol/claude-agent-acp currently requires ANTHROPIC_API_KEY.
    // OAuth (Claude Max subscription) is NOT supported. Use ClaudeExec when you're
    // signed in via `claude` and don't have an API key set.
    return {
      command: "npx",
      args: ["-y", "@agentclientprotocol/claude-agent-acp@0.35.0"],
    };
  }
}
