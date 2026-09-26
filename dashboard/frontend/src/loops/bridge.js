// The webview side of the loop bridge (dashboard/src-tauri/src/bridge). Tauri globals
// (`withGlobalTauri`), so no @tauri-apps/api dependency — the same way app.js talks to
// the backend. Outside Tauri (vite dev, node tests) calls fail with code "no_tauri".

function tauri() {
  return typeof window !== "undefined" ? window.__TAURI__ : undefined;
}

export function available() {
  return !!tauri()?.core?.invoke;
}

// Bridge errors arrive as {code, message, details?}; anything else becomes one.
export function asError(e) {
  if (e && typeof e === "object" && typeof e.code === "string") {
    return { code: e.code, message: String(e.message || e.code), details: e.details };
  }
  return { code: "error", message: String(e?.message || e) };
}

export async function call(cmd, args) {
  const invoke = tauri()?.core?.invoke;
  if (!invoke) throw { code: "no_tauri", message: "the desktop bridge is not available" };
  try {
    return await invoke(cmd, args);
  } catch (e) {
    throw asError(e);
  }
}

// Subscribe to a bridge event; returns an unsubscribe function (safe to call early,
// before the async listen resolves).
export function on(event, handler) {
  const listen = tauri()?.event?.listen;
  if (!listen) return () => {};
  let off = null;
  let cancelled = false;
  listen(event, (e) => handler(e.payload)).then((un) => {
    if (cancelled) un();
    else off = un;
  });
  return () => {
    cancelled = true;
    if (off) off();
  };
}

// Commands (names and argument keys match bridge/commands.rs and blueprints.rs; Tauri
// maps camelCase arguments onto the snake_case parameters).
export const board = () => call("loops_board");
export const openLoop = (loopId) => call("loop_open", { loopId });
export const closeLoop = (loopId) => call("loop_close", { loopId });
export const startLoop = (request) => call("loop_start", { request });
export const control = (loopId, op, opId) => call("loop_control", { loopId, op, opId });
export const resolveGate = (loopId, gateId, option, comment, opId) =>
  call("gate_resolve", { loopId, gateId, option, comment, opId });
export const stepOutput = (loopId, stepId, what, offset, maxBytes) =>
  call("step_output", { loopId, stepId, what, offset, maxBytes });
export const blueprints = (project) => call("blueprints_list", { project: project || null });
export const getBlueprint = (scope, name, project) =>
  call("blueprint_get", { scope, name, project: project || null });
export const saveBlueprint = (scope, name, doc, baseRev, project) =>
  call("blueprint_save", { scope, name, doc, baseRev: baseRev || null, project: project || null });
export const deleteBlueprint = (scope, name, baseRev, project) =>
  call("blueprint_delete", { scope, name, baseRev, project: project || null });
export const validateBlueprint = (doc) => call("blueprint_validate", { doc });
export const generateBlueprint = (description, cwd) =>
  call("blueprint_generate", { description, cwd: cwd || null });
export const pendingAsks = () => call("get_pending_asks");
export const answerAsk = (askId, value) => call("answer_ask", { askId, value });

// A fresh op id (a canonical UUID), so a retried click never applies twice.
export function newOpId() {
  if (typeof crypto !== "undefined" && crypto.randomUUID) return crypto.randomUUID();
  const h = () => Math.floor(Math.random() * 0x10000).toString(16).padStart(4, "0");
  return `${h()}${h()}-${h()}-4${h().slice(1)}-a${h().slice(1)}-${h()}${h()}${h()}`;
}
