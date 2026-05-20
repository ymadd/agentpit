import { z } from "zod";

export const BackendIdSchema = z.enum([
  "claude",
  "codex",
  "gemini",
  "opencode",
  "goose",
  "copilot",
]);

export type BackendId = z.infer<typeof BackendIdSchema>;

export type ToolSource = BackendId | `${BackendId}:aggregator`;

export interface ProgressEmitter {
  emit(payload: {
    source: ToolSource;
    text?: string;
    update?: unknown;
  }): Promise<void>;
}

export interface HubSession {
  readonly id: string;
  readonly cwd: string;
  readonly createdAt: number;
  readonly currentBackend?: BackendId;
}

export interface BackendSessionInfo {
  readonly backendSessionId: string;
  readonly lastUsedAt: number;
}
