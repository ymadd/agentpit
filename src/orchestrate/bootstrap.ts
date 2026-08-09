// Orchestration REPL bootstrap (design §10.2) — embedded into the agentpit binary and
// run as: deno run --allow-read=<cwd>,<artifacts> --allow-write=<artifacts> bootstrap.ts
//
// NDJSON over stdio. Frames FROM the host: {type:"cell", id, code}. Frames TO the host:
// {type:"cell_result", id, ok, repr, error?} and {type:"host_call", id, fn, args} (which
// the host answers with {type:"host_result", id, ok, value, error?}).
//
// Cell contract (mirrored in the declaration header the host prepends for `deno check`):
// - Persist values across cells on the scope object: `S.x = await dispatch(...)`.
//   `const`/`let` are cell-local by design (each cell is its own block in both the
//   typechecked module and this runtime).
// - End a cell with `return <expr>` to display a truncated repr of the value; the full
//   value stays in the deno heap (context-as-variable, §10.6).
// - The only exits to the world are the host functions below — there is no fs/net/run
//   permission in this process beyond the artifacts dir.

type HostFrame =
  | { type: "cell"; id: number; code: string }
  | { type: "host_result"; id: number; ok: boolean; value?: unknown; error?: string };

const enc = new TextEncoder();

function writeLine(obj: unknown): void {
  Deno.stdout.writeSync(enc.encode(JSON.stringify(obj) + "\n"));
}

// stdout is the protocol channel ONLY. Redirect the cell's console.* to stderr so a
// `console.log('{"type":"cell_result",…}')` cannot forge a protocol frame (H5). Direct
// Deno.stdout writes from cell code could still forge, but `--no-remote` keeps cells to
// the user's own code, and the host also matches cell_result ids — so an accidental
// collision can't happen and a deliberate one is self-inflicted.
function toStderr(...parts: unknown[]): void {
  const text = parts
    .map((p) => (typeof p === "string" ? p : Deno.inspect(p)))
    .join(" ");
  Deno.stderr.writeSync(enc.encode(text + "\n"));
}
globalThis.console.log = toStderr;
globalThis.console.info = toStderr;
globalThis.console.warn = toStderr;
globalThis.console.error = toStderr;
globalThis.console.debug = toStderr;

// ── host-call plumbing ──────────────────────────────────────────────────────
let nextHostId = 1;
const pendingHost = new Map<
  number,
  { resolve: (v: unknown) => void; reject: (e: Error) => void }
>();

function hostCall(fn: string, args: unknown): Promise<unknown> {
  const id = nextHostId++;
  return new Promise((resolve, reject) => {
    pendingHost.set(id, { resolve, reject });
    writeLine({ type: "host_call", id, fn, args });
  });
}

// ── the API surface cells see (globals; typed in the decl header) ───────────
const S: Record<string, unknown> = {};

async function dispatch(
  task: string,
  opts?: { backend?: string },
): Promise<{ backend: string; status: string; answer: string }> {
  return (await hostCall("dispatch", {
    task,
    backend: opts?.backend ?? null,
  })) as { backend: string; status: string; answer: string };
}

const store = {
  async put(key: string, value: unknown): Promise<void> {
    await hostCall("store_put", { key, value });
  },
  async get(key: string): Promise<unknown> {
    return await hostCall("store_get", { key });
  },
  async list(): Promise<string[]> {
    return (await hostCall("store_list", {})) as string[];
  },
};

const session = {
  /** Recent (who, text) turns from the session log, oldest first. */
  async answers(n?: number): Promise<[string, string][]> {
    return (await hostCall("session_answers", { n: n ?? 10 })) as [
      string,
      string,
    ][];
  },
};

/** Full (untruncated) view of a value, up to `n` chars — the escape hatch from reprs. */
function preview(value: unknown, n?: number): string {
  const s = typeof value === "string" ? value : JSON.stringify(value, null, 1);
  return (s ?? "undefined").slice(0, n ?? 2000);
}

// Anchor the API into the global scope for eval'd cells.
Object.assign(globalThis, { S, dispatch, store, session, preview });

// ── reprs (§10.6): head + TOTAL SIZE so the model knows there is more ───────
function repr(value: unknown): string {
  if (value === undefined) return "undefined";
  if (value === null) return "null";
  if (typeof value === "string") {
    return value.length <= 200
      ? JSON.stringify(value)
      : `[string ${value.length} chars] ${JSON.stringify(value.slice(0, 200))}…`;
  }
  if (typeof value === "number" || typeof value === "boolean") {
    return String(value);
  }
  if (Array.isArray(value)) {
    const head = value.length ? repr(value[0]) : "";
    return `[array length ${value.length}]${head ? ` first: ${head}` : ""}`;
  }
  if (typeof value === "object") {
    const keys = Object.keys(value as object);
    const shown = keys.slice(0, 12).join(", ");
    let body = "";
    try {
      const json = JSON.stringify(value);
      body =
        json.length <= 200
          ? ` ${json}`
          : ` [json ${json.length} chars] ${json.slice(0, 200)}…`;
    } catch {
      body = " [unserializable]";
    }
    return `{keys: ${shown}${keys.length > 12 ? ", …" : ""}}${body}`;
  }
  return Object.prototype.toString.call(value);
}

// ── cell evaluation ─────────────────────────────────────────────────────────
// Cells are TYPESCRIPT: `eval` would reject type annotations at runtime (found in the
// first live smoke — the typecheck accepted what eval then refused), so each cell is
// materialized as a module under the artifacts dir and dynamically imported — Deno
// transpiles it transparently. The wrapper function keeps `return <expr>` legal and
// const/let cell-local; persistence stays on the global S.
const artifactsDir = Deno.args[0] ?? ".";

async function evalCell(id: number, code: string): Promise<void> {
  try {
    const file = `${artifactsDir}/checks/cell-${id}.ts`;
    await Deno.writeTextFile(
      file,
      `export default async function (): Promise<unknown> {\n${code}\n}\n`,
    );
    const mod = await import(
      "file://" + (file.startsWith("/") ? file : `${Deno.cwd()}/${file}`)
    );
    const value = await mod.default();
    writeLine({ type: "cell_result", id, ok: true, repr: repr(value) });
  } catch (e) {
    writeLine({
      type: "cell_result",
      id,
      ok: false,
      repr: "",
      error: e instanceof Error ? `${e.name}: ${e.message}` : String(e),
    });
  }
}

// ── stdin pump ──────────────────────────────────────────────────────────────
const decoder = new TextDecoder();
let buffer = "";
for await (const chunk of Deno.stdin.readable) {
  buffer += decoder.decode(chunk, { stream: true });
  let nl: number;
  while ((nl = buffer.indexOf("\n")) >= 0) {
    const line = buffer.slice(0, nl).trim();
    buffer = buffer.slice(nl + 1);
    if (!line) continue;
    let frame: HostFrame;
    try {
      frame = JSON.parse(line) as HostFrame;
    } catch {
      continue;
    }
    if (frame.type === "cell") {
      // Cells run sequentially by construction: the host sends the next cell only
      // after this one's result. Host calls interleave freely within a cell.
      evalCell(frame.id, frame.code);
    } else if (frame.type === "host_result") {
      const pending = pendingHost.get(frame.id);
      if (pending) {
        pendingHost.delete(frame.id);
        if (frame.ok) pending.resolve(frame.value);
        else pending.reject(new Error(frame.error ?? "host call failed"));
      }
    }
  }
}
