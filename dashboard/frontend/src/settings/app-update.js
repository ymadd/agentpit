const AUTO_UPDATE_KEY = "agentpit.desktop.auto-update";

let started = false;
let snapshot = {
  status: "idle",
  info: null,
  message: "",
  error: null,
};
const listeners = new Set();

function publish(patch) {
  snapshot = { ...snapshot, ...patch };
  for (const listener of listeners) listener(snapshot);
}

function invoke() {
  return window.__TAURI__?.core?.invoke;
}

export function getUpdateSnapshot() {
  return snapshot;
}

export function subscribeToUpdates(listener) {
  listeners.add(listener);
  listener(snapshot);
  return () => listeners.delete(listener);
}

export function autoUpdateEnabled() {
  try {
    return localStorage.getItem(AUTO_UPDATE_KEY) !== "false";
  } catch {
    return true;
  }
}

export function setAutoUpdateEnabled(enabled) {
  try {
    localStorage.setItem(AUTO_UPDATE_KEY, enabled ? "true" : "false");
  } catch {
    // A webview with blocked storage still uses the in-session setting.
  }
  window.dispatchEvent(new CustomEvent("agentpit:auto-update-setting", { detail: !!enabled }));
}

export async function installAvailableUpdate() {
  const call = invoke();
  if (!call) {
    publish({ status: "unavailable", error: "デスクトップ版でのみ更新できます。" });
    return snapshot;
  }
  publish({ status: "installing", message: "更新をダウンロードしています…", error: null });
  try {
    const result = await call("app_update_install");
    publish({
      info: snapshot.info ? {
        ...snapshot.info,
        available: false,
        bundled_cli_version: result?.installed_version || snapshot.info.bundled_cli_version,
      } : snapshot.info,
      status: result?.restart_required ? "restart" : "current",
      message: result?.output || "更新をインストールしました。",
      error: null,
    });
  } catch (error) {
    publish({ status: "error", error: String(error), message: "" });
  }
  return snapshot;
}

export async function checkForUpdates({ installIfAvailable = false } = {}) {
  const call = invoke();
  if (!call) {
    publish({ status: "unavailable", error: null, message: "ブラウザプレビューでは更新確認を行いません。" });
    return snapshot;
  }
  publish({ status: "checking", error: null, message: "最新版を確認しています…" });
  try {
    const info = await call("app_update_check");
    publish({
      info,
      status: info.available ? "available" : "current",
      message: info.available ? `v${info.latest_version} を利用できます。` : "最新版です。",
      error: null,
    });
    if (info.available && installIfAvailable) return installAvailableUpdate();
  } catch (error) {
    publish({ status: "error", error: String(error), message: "" });
  }
  return snapshot;
}

export async function restartDesktopApp() {
  const call = invoke();
  if (call) await call("app_restart");
}

export function startAutoUpdater() {
  if (started) return;
  started = true;
  window.setTimeout(() => {
    checkForUpdates({ installIfAvailable: autoUpdateEnabled() });
  }, 1200);
}
