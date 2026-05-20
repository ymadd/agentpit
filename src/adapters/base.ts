import { spawn, type ChildProcess } from "node:child_process";
import { Readable, Writable } from "node:stream";
import {
  ClientSideConnection,
  ndJsonStream,
  PROTOCOL_VERSION,
  type Agent,
  type Client,
  type SessionNotification,
  type StopReason,
} from "@agentclientprotocol/sdk";

import type { BackendId } from "../types.js";
import { readTextFile, writeTextFile } from "../filesystem-bridge.js";
import {
  defaultPolicy,
  handleRequestPermission,
  type PermissionPolicy,
} from "../permission-bridge.js";

export interface SpawnSpec {
  readonly command: string;
  readonly args: ReadonlyArray<string>;
  readonly env?: Readonly<Record<string, string | undefined>>;
}

export interface AdapterOptions {
  readonly policy?: PermissionPolicy;
  readonly onUpdate?: (notification: SessionNotification) => void;
}

export abstract class BaseAdapter {
  abstract readonly id: BackendId;
  protected abstract spawnSpec(): SpawnSpec;

  private proc?: ChildProcess;
  private connection?: ClientSideConnection;
  private readonly policy: PermissionPolicy;
  private onUpdateListener?: (n: SessionNotification) => void;

  constructor(options: AdapterOptions = {}) {
    this.policy = options.policy ?? defaultPolicy;
    this.onUpdateListener = options.onUpdate;
  }

  setUpdateListener(listener: (n: SessionNotification) => void): void {
    this.onUpdateListener = listener;
  }

  async ensureRunning(): Promise<void> {
    if (this.connection) return;

    const { command, args, env } = this.spawnSpec();
    const child = spawn(command, [...args], {
      stdio: ["pipe", "pipe", "inherit"],
      env: { ...process.env, ...env },
    });

    child.on("error", (error) => {
      console.error(`[agentpit] backend ${this.id} spawn error:`, error);
    });

    if (!child.stdin || !child.stdout) {
      throw new Error(`Backend ${this.id} stdio not available`);
    }

    this.proc = child;

    const stdinWeb = Writable.toWeb(child.stdin) as WritableStream<Uint8Array>;
    const stdoutWeb = Readable.toWeb(
      child.stdout,
    ) as ReadableStream<Uint8Array>;
    const stream = ndJsonStream(stdinWeb, stdoutWeb);

    const clientImpl: Client = {
      requestPermission: (params) =>
        handleRequestPermission(params, this.policy),
      sessionUpdate: async (params) => {
        this.onUpdateListener?.(params);
      },
      readTextFile,
      writeTextFile,
    };

    const connection = new ClientSideConnection(
      (_agent: Agent) => clientImpl,
      stream,
    );

    await connection.initialize({
      protocolVersion: PROTOCOL_VERSION,
      clientCapabilities: {
        fs: { readTextFile: true, writeTextFile: true },
        terminal: false,
      },
    });

    this.connection = connection;
  }

  async newSession(cwd: string): Promise<string> {
    await this.ensureRunning();
    const response = await this.connection!.newSession({
      cwd,
      mcpServers: [],
    });
    return response.sessionId;
  }

  async prompt(
    sessionId: string,
    text: string,
    onChunk: (chunk: string) => void,
  ): Promise<StopReason> {
    await this.ensureRunning();
    const prevListener = this.onUpdateListener;
    this.onUpdateListener = (notification) => {
      const update = notification.update;
      if (
        update.sessionUpdate === "agent_message_chunk" &&
        update.content.type === "text"
      ) {
        onChunk(update.content.text);
      }
      prevListener?.(notification);
    };
    try {
      const response = await this.connection!.prompt({
        sessionId,
        prompt: [{ type: "text", text }],
      });
      return response.stopReason;
    } finally {
      this.onUpdateListener = prevListener;
    }
  }

  async cancel(sessionId: string): Promise<void> {
    if (!this.connection) return;
    await this.connection.cancel({ sessionId });
  }

  async close(): Promise<void> {
    const proc = this.proc;
    if (!proc || proc.killed) return;

    proc.kill("SIGTERM");
    await new Promise<void>((resolve) => {
      const timer = setTimeout(() => {
        if (!proc.killed) proc.kill("SIGKILL");
        resolve();
      }, 3000);
      proc.once("exit", () => {
        clearTimeout(timer);
        resolve();
      });
    });
    this.proc = undefined;
    this.connection = undefined;
  }
}
