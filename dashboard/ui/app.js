// agentpit dashboard — the decision cockpit.
//
// One manager supervises the swarm; this window surfaces EXACTLY the things that need a human,
// one at a time. When nothing does, it says so (inbox zero). The swarm is opt-in (footer).
//
// Live data:
//   get_pending_asks()  -> the decision queue (each ask = one decision card)
//   answer_ask(id,value)-> the human's reply, delivered to the blocked manager
//   get_snapshot()      -> the swarm (in-flight runs, grouped by project = cwd)

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

// ── state ────────────────────────────────────────────────────────────────────
let asks = []; // pending decisions (camelCase from get_pending_asks)
let snapshot = { live: [], recent: [] };
let available = true; // presence toggle (cosmetic — the manager handles real absence via timeout)
let showSwarm = false;
let showCliManager = false;
let projectFilter = null;
let cursor = 0; // which pending ask is on stage
let connected = false;
let toastTimer = null;
let agentClis = [];
let cliLoading = false;
let cliUpdating = null;
let cliManagerError = null;

const answered = new Set(); // optimistically-answered ids, hidden until the backend agrees
const notified = new Set(); // blocking asks we've already notified for

// ── helpers ──────────────────────────────────────────────────────────────────
function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
}
function basename(p) {
  if (!p) return "";
  const parts = p.replace(/\/+$/, "").split("/");
  return parts[parts.length - 1] || p;
}
function hashHue(s) {
  let h = 0;
  for (let i = 0; i < (s || "").length; i++) h = (h * 31 + s.charCodeAt(i)) >>> 0;
  return h % 360;
}
function projectColor(name) {
  return `oklch(0.72 0.07 ${hashHue(name || "")})`;
}
const MODEL = {
  claude: { mono: "C", color: "#d98a6b" },
  codex: { mono: "co", color: "#56b89a" },
  gemini: { mono: "G", color: "#6f8fd9" },
  antigravity: { mono: "ag", color: "#c9a227" },
  opencode: { mono: "oc", color: "#7c5cff" },
  goose: { mono: "go", color: "#a7adba" },
  copilot: { mono: "cp", color: "#7d8595" },
};
function modelMeta(b) {
  return MODEL[b] || { mono: "•", color: "#7d8595" };
}
const KIND = {
  rescue: "rescue",
  review: "review",
  security_review: "security",
  adversarial_review: "adversarial",
  explain: "explain",
  refactor: "refactor",
  ensemble: "ensemble",
  workflow: "workflow",
};
function kindLabel(k) {
  return KIND[k] || k || "run";
}
function fmtChars(n) {
  if (n == null) return "—";
  return n < 1024 ? String(n) : `${(n / 1024).toFixed(1)}K`;
}
function fmtElapsed(runTs, m) {
  const ts = (m && m.started_ts) || runTs;
  if (!ts) return "稼働";
  const s = Math.max(0, Math.floor((Date.now() - ts) / 1000));
  return s < 60 ? `${s}s` : `${Math.floor(s / 60)}m`;
}
function visibleAsks() {
  return asks;
}
function runIndex() {
  const idx = {};
  for (const r of [...(snapshot.live || []), ...(snapshot.recent || [])]) idx[r.run_id] = r;
  return idx;
}
function swarmCounts() {
  const runs = snapshot.live || [];
  const projs = new Set();
  let total = 0;
  let running = 0;
  for (const r of runs) {
    projs.add(basename(r.cwd) || r.run_id);
    for (const m of r.members || []) {
      total++;
      if (m.status === "running") running++;
    }
  }
  return { projects: projs.size, total, running };
}

// ── render orchestration ─────────────────────────────────────────────────────
function renderAll() {
  updateStatusbar();
  updateStage();
  updateFooter();
  updateSwarm();
  updateCliManager();
}

function updateStatusbar() {
  const counts = swarmCounts();
  const ml = document.getElementById("manager-line");
  if (!available) ml.textContent = "離席中 — 群れは安全側で続行します";
  else if (counts.projects === 0) ml.textContent = "群れは静かです";
  else ml.textContent = `マネージャー1体が ${counts.projects} プロジェクトを束ねています`;

  const list = visibleAsks();
  document.getElementById("pending-count").textContent = list.length;
  document.getElementById("pending-pill").classList.toggle("hidden", list.length === 0);

  const at = document.getElementById("avail-toggle");
  at.classList.toggle("away", !available);
  at.querySelector(".avail-label").textContent = available ? "在席中" : "離席中";
}

function updateFooter() {
  const counts = swarmCounts();
  document.getElementById("swarm-footer").textContent = `${counts.projects} プロジェクト · ${counts.total} 体`;
  const conn = document.getElementById("conn");
  conn.classList.toggle("live", connected);
  document.getElementById("conn-text").textContent = connected ? "接続中" : "未接続";
}

// stage = idle (inbox zero) OR exactly one decision card
let stageSig = null;
function updateStage() {
  const list = visibleAsks();
  const idleEl = document.getElementById("idle");
  const decEl = document.getElementById("decision");
  if (list.length === 0) {
    decEl.classList.add("hidden");
    const c = swarmCounts();
    const sig = `idle|${available}|${c.running}|${c.projects}`;
    if (sig !== stageSig) {
      buildIdle(idleEl, c);
      stageSig = sig;
    }
    idleEl.classList.remove("hidden");
  } else {
    idleEl.classList.add("hidden");
    if (cursor >= list.length) cursor = list.length - 1;
    if (cursor < 0) cursor = 0;
    const sig = `dec|${list.map((x) => x.askId).join(",")}|${cursor}`;
    if (sig !== stageSig) {
      buildDecision(decEl, list[cursor], list);
      stageSig = sig;
    }
    decEl.classList.remove("hidden");
  }
}

function buildIdle(root, counts) {
  root.innerHTML = "";
  const wrap = el("div", "idle-wrap");

  const orb = el("div", "idle-orb");
  const dotColor = available ? "#3fb950" : "#6b7484";
  const core = el("span", "core");
  const breathe = el("span", "breathe");
  core.style.background = dotColor;
  breathe.style.background = dotColor;
  orb.append(core, breathe);

  const eyebrow = el("div", "idle-eyebrow", available ? "INBOX ZERO" : "AWAY");
  eyebrow.style.color = available ? "#3a8a55" : "#6b7484";

  const h = el(
    "h1",
    "idle-headline",
    available ? "あなたの判断を待っているものは、ありません。" : "離れています。群れは止まりません。"
  );
  const sub = el(
    "p",
    "idle-sub",
    available
      ? `${counts.running} 体のエージェントが ${counts.projects} プロジェクトで静かに走っています。人にしか決められないことが起きたとき、ここに一つだけ現れます。それまでは、離れていて構いません。`
      : "あなたが不在でも、群れは安全側で進み続けます。人にしか決められないことは保留し、戻ったときにまとめて報告します。"
  );
  wrap.append(orb, eyebrow, h, sub);

  if (available) {
    const chips = el("div", "chips");
    for (const t of ["O(1)", "単一窓口", "止まらない"]) chips.appendChild(el("span", "chip", t));
    wrap.appendChild(chips);
  }
  root.appendChild(wrap);
}

function buildDecision(root, a, list) {
  root.innerHTML = "";
  const run = runIndex()[a.runId];
  const cwd = run ? run.cwd : "";
  const project = basename(cwd) || "—";
  const blocking = a.kind === "blocking";

  const scroll = el("div", "dec-scroll");
  const wrap = el("div", "dec-wrap");

  // header: project · kind · dir | queue · badge
  const head = el("div", "dec-head");
  const hl = el("div", "dec-head-l");
  const pdot = el("span", "dec-proj-dot");
  pdot.style.background = projectColor(project);
  hl.append(pdot, el("span", "dec-proj-name", project), el("span", "dec-kind", run ? kindLabel(run.kind) : a.kind));
  if (cwd) hl.appendChild(el("span", "dec-dir", cwd));
  const hr = el("div", "dec-head-r");
  if (list.length > 1) hr.appendChild(el("span", "dec-queue", `${cursor + 1} / ${list.length}`));
  const badge = el("div", "dec-badge " + (blocking ? "blocking" : "review"));
  badge.append(el("span", "d"), el("span", "l", blocking ? "要対応" : "確認"));
  hr.appendChild(badge);
  head.append(hl, hr);
  wrap.appendChild(head);

  // the card
  const card = el("div", "dec-card");
  card.appendChild(el("h2", "dec-title", a.prompt));
  card.appendChild(
    el("p", "dec-reason", blocking ? "ワーカーが停止して、あなたの判断を待っています。" : "判断を確認したいことがあります。")
  );
  const shortId = (a.askId || "").replace(/^ask-/, "").slice(0, 14);
  card.appendChild(el("div", "dec-context", `${shortId}  ·  ${a.timeoutSecs}s 応答がなければ安全側で続行`));

  const actions = el("div", "dec-actions");
  const opts = a.options && a.options.length ? a.options : ["yes", "no"];
  const isYesNo = !(a.options && a.options.length);
  opts.forEach((opt, i) => {
    const btn = el("button", "dec-btn " + (i === 0 ? "approve" : "neutral"));
    btn.type = "button";
    const key = isYesNo ? (i === 0 ? "Y" : "N") : String(i + 1);
    const label = isYesNo ? (i === 0 ? "はい" : "いいえ") : opt;
    btn.append(el("span", "key", key), el("span", "lab", label));
    btn.addEventListener("click", () => answer(a.askId, opt));
    actions.appendChild(btn);
  });
  card.appendChild(actions);
  wrap.appendChild(card);

  // reassurance
  const foot = el("div", "dec-foot");
  let reassure = "待つ間も、可逆な作業は止まりません";
  if (list.length > 1) {
    const next = list[(cursor + 1) % list.length];
    const nextRun = runIndex()[next.runId];
    const nextProj = (nextRun && basename(nextRun.cwd)) || next.runId;
    reassure = `待つ間も可逆作業は継続中 · 次は ${nextProj}`;
  }
  foot.appendChild(el("span", "reassure", reassure));
  wrap.appendChild(foot);

  scroll.appendChild(wrap);
  root.appendChild(scroll);
}

// ── swarm glance (opt-in) ────────────────────────────────────────────────────
let swarmSig = null;
function updateSwarm() {
  const root = document.getElementById("swarm");
  if (!showSwarm) {
    root.classList.add("hidden");
    swarmSig = null;
    return;
  }
  const runs = snapshot.live || [];
  const sig =
    `${projectFilter}|` +
    runs.map((r) => r.run_id + ":" + (r.members || []).map((m) => m.backend + m.status).join(",")).join("|");
  if (sig === swarmSig) return;
  swarmSig = sig;
  buildSwarm(root, runs);
  root.classList.remove("hidden");
}

function railItem(label, filterVal, count, active) {
  const b = el("button", "rail-item" + (active ? " active" : ""));
  b.type = "button";
  const d = el("span", "dot");
  d.style.background = filterVal ? projectColor(filterVal) : "transparent";
  b.append(d, el("span", "name", label), el("span", "count", String(count)));
  b.addEventListener("click", () => {
    projectFilter = filterVal;
    swarmSig = null;
    updateSwarm();
  });
  return b;
}

function workerRow(run, m) {
  const mm = modelMeta(m.backend);
  const row = el("div", "worker");
  const av = el("span", "avatar", mm.mono);
  av.style.background = mm.color;

  const body = el("div", "body");
  const top = el("div", "top");
  top.append(
    el("span", "label", `${kindLabel(run.kind)} / ${m.backend}${m.aggregator ? " ·agg" : ""}`),
    el("span", "model", m.backend)
  );
  body.append(top, el("div", "task", run.cwd || "—"));

  const right = el("div", "right");
  right.appendChild(el("span", "tokens", fmtChars(m.chars)));
  const stat = el("div", "stat");
  const running = m.status === "running";
  const sl = el("span", "sl");
  const sd = el("span", "sd");
  if (running) {
    sl.textContent = fmtElapsed(run.started_ts, m);
    sl.style.color = "#6b7484";
    sd.style.background = "var(--ac)";
    sd.style.animation = "ap-pulse 2.2s ease-in-out infinite";
  } else if (m.status === "ok") {
    sl.textContent = "完了";
    sl.style.color = "#3f8f6f";
    sd.style.background = "#3f8f6f";
  } else if (m.status === "error" || m.status === "interrupted") {
    sl.textContent = "失敗";
    sl.style.color = "var(--err)";
    sd.style.background = "var(--err)";
  } else {
    sl.textContent = m.status || "待機";
    sl.style.color = "var(--muted-2)";
    sd.style.background = "var(--muted-2)";
  }
  stat.append(sl, sd);
  right.appendChild(stat);

  row.append(av, body, right);
  return row;
}

function buildSwarm(root, runs) {
  root.innerHTML = "";
  const scrim = el("div", "swarm-scrim");
  scrim.addEventListener("click", toggleSwarm);
  const sheet = el("div", "swarm-sheet");

  // group members by project (= cwd basename)
  const groups = new Map();
  for (const r of runs) {
    const proj = basename(r.cwd) || r.run_id;
    for (const m of r.members || []) {
      if (!groups.has(proj)) groups.set(proj, []);
      groups.get(proj).push({ run: r, m });
    }
  }
  const projects = [...groups.keys()];
  const totalAll = runs.reduce((a, r) => a + (r.members || []).length, 0);
  const runningAll = runs.reduce((a, r) => a + (r.members || []).filter((m) => m.status === "running").length, 0);

  // head
  const head = el("div", "swarm-head");
  const ht = el("div");
  const title = el("div", "swarm-title");
  title.appendChild(el("span", "t", "群れ"));
  const filteredItems = projectFilter ? groups.get(projectFilter) || [] : null;
  const headCount = projectFilter
    ? `${filteredItems.filter((x) => x.m.status === "running").length} 体稼働 / ${filteredItems.length} 体`
    : `${projects.length} プロジェクト · ${runningAll} 体稼働 / 全 ${totalAll} 体`;
  title.appendChild(el("span", "c", headCount));
  ht.append(title, el("div", "swarm-sub", "ここは見なくて大丈夫。マネージャーが見ています。"));
  const close = el("button", "swarm-close");
  close.type = "button";
  close.append(el("span", "l", "閉じる"), el("span", "x", "✕"));
  close.addEventListener("click", toggleSwarm);
  head.append(ht, close);
  sheet.appendChild(head);

  // body: rail + main
  const body = el("div", "swarm-body");
  const rail = el("div", "swarm-rail");
  rail.appendChild(el("div", "rail-head", "PROJECTS"));
  const railList = el("div", "rail-list");
  railList.appendChild(railItem("すべて", null, totalAll, projectFilter === null));
  for (const p of projects) railList.appendChild(railItem(p, p, (groups.get(p) || []).length, projectFilter === p));
  rail.append(railList);
  body.appendChild(rail);

  const main = el("div", "swarm-main");
  const scroll = el("div", "swarm-scroll");
  const showProjects = projectFilter ? projects.filter((p) => p === projectFilter) : projects;
  if (showProjects.length === 0) scroll.appendChild(el("div", "swarm-empty", "いま走っている群れはありません。"));
  for (const p of showProjects) {
    const items = groups.get(p) || [];
    const g = el("div", "swarm-group");
    const gh = el("div", "group-head");
    const gd = el("span", "dot");
    gd.style.background = projectColor(p);
    gh.append(
      gd,
      el("span", "name", p),
      el("span", "meta", `${items.filter((x) => x.m.status === "running").length} / ${items.length} 稼働`)
    );
    g.appendChild(gh);
    for (const { run, m } of items) g.appendChild(workerRow(run, m));
    scroll.appendChild(g);
  }
  main.appendChild(scroll);
  body.appendChild(main);
  sheet.appendChild(body);

  root.append(scrim, sheet);
}

// ── agent CLI versions ───────────────────────────────────────────────────────
function updateCliManager() {
  const root = document.getElementById("cli-manager");
  if (!showCliManager) {
    root.classList.add("hidden");
    return;
  }
  buildCliManager(root);
  root.classList.remove("hidden");
}

function buildCliManager(root) {
  root.innerHTML = "";
  const scrim = el("div", "cli-scrim");
  scrim.addEventListener("click", toggleCliManager);
  const panel = el("section", "cli-panel");
  panel.setAttribute("aria-label", "Agent CLI バージョン管理");

  const head = el("header", "cli-head");
  const intro = el("div", "cli-intro");
  intro.append(
    el("div", "cli-eyebrow", "TOOLCHAIN / LOCAL"),
    el("h2", "cli-title", "Agent CLI versions"),
    el("p", "cli-sub", "agentpit が実際に呼び出す CLI を確認し、各 CLI 公式の更新機能で揃えます。")
  );
  const headActions = el("div", "cli-head-actions");
  const refresh = el("button", "cli-refresh", cliLoading ? "確認中…" : "再確認");
  refresh.type = "button";
  refresh.disabled = cliLoading || cliUpdating !== null;
  refresh.addEventListener("click", fetchAgentClis);
  const close = el("button", "cli-close", "✕");
  close.type = "button";
  close.setAttribute("aria-label", "閉じる");
  close.addEventListener("click", toggleCliManager);
  headActions.append(refresh, close);
  head.append(intro, headActions);
  panel.appendChild(head);

  const list = el("div", "cli-list");
  if (cliManagerError) {
    const error = el("div", "cli-error");
    error.append(el("strong", null, "更新できませんでした"), el("span", null, cliManagerError));
    list.appendChild(error);
  }
  if (cliLoading && agentClis.length === 0) {
    list.appendChild(el("div", "cli-empty", "ローカルの CLI を確認しています…"));
  } else {
    for (const cli of agentClis) list.appendChild(cliRow(cli));
  }
  panel.appendChild(list);

  const foot = el("footer", "cli-foot");
  const installed = agentClis.filter((cli) => cli.installed).length;
  foot.append(
    el("span", "cli-summary", `${installed} / ${agentClis.length || 5} installed`),
    el("span", "cli-safety", "更新コマンドは固定されています。任意のシェル入力は実行しません。")
  );
  panel.appendChild(foot);
  root.append(scrim, panel);
}

function cliRow(cli) {
  const meta = modelMeta(cli.id);
  const row = el("article", `cli-row${cli.installed ? "" : " missing"}`);
  const mark = el("span", "cli-mark", meta.mono);
  mark.style.setProperty("--cli-color", meta.color);

  const identity = el("div", "cli-identity");
  const nameLine = el("div", "cli-name-line");
  nameLine.append(el("span", "cli-name", cli.label));
  const state = el("span", `cli-state ${cli.installed ? "ready" : "missing"}`, cli.installed ? "installed" : "missing");
  nameLine.appendChild(state);
  identity.append(nameLine, el("div", "cli-path mono", cli.path || `${cli.command} is not on PATH`));
  if (cli.note) identity.appendChild(el("div", "cli-note", cli.note));

  const version = el("div", "cli-version");
  version.append(el("span", "cli-version-label", "VERSION"), el("strong", "mono", cli.version || "—"));

  const updating = cliUpdating === cli.id;
  const action = el("button", "cli-update", updating ? "更新中…" : "更新");
  action.type = "button";
  action.disabled = !cli.canUpdate || cliUpdating !== null;
  action.title = cli.canUpdate ? cli.updateCommand || "更新" : cli.note || "更新できません";
  action.addEventListener("click", () => updateAgentCli(cli));

  row.append(mark, identity, version, action);
  return row;
}

// ── interaction ──────────────────────────────────────────────────────────────
function showToast(text, dotColor) {
  const t = document.getElementById("toast");
  t.innerHTML = "";
  const d = el("span", "dot");
  d.style.background = dotColor;
  t.append(d, el("span", "text", text));
  t.classList.remove("hidden");
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => t.classList.add("hidden"), 3200);
}

async function answer(askId, value) {
  // Optimistic: drop the card immediately so the keystroke feels instant.
  answered.add(askId);
  asks = asks.filter((a) => a.askId !== askId);
  showToast(`「${value}」を伝えました。群れに反映します。`, "#4ec9a0");
  renderAll();
  try {
    await invoke("answer_ask", { askId, value });
  } catch (e) {
    /* the next fetch reconciles */
  }
  fetchAsks();
}

function answerKey(k) {
  const list = visibleAsks();
  const a = list[cursor];
  if (!a) return false;
  const opts = a.options && a.options.length ? a.options : ["yes", "no"];
  const isYesNo = !(a.options && a.options.length);
  const key = k.toLowerCase();
  if (key === "enter") {
    answer(a.askId, opts[0]);
    return true;
  }
  if (isYesNo) {
    if (key === "y") {
      answer(a.askId, opts[0]);
      return true;
    }
    if (key === "n" || key === "escape") {
      answer(a.askId, opts[1]);
      return true;
    }
  } else if (key >= "1" && key <= "9") {
    const idx = Number(key) - 1;
    if (idx < opts.length) {
      answer(a.askId, opts[idx]);
      return true;
    }
  }
  return false;
}

function onKey(e) {
  if (showCliManager) {
    if (e.key === "Escape") {
      toggleCliManager();
      e.preventDefault();
    }
    return;
  }
  if (showSwarm) {
    if (e.key === "Escape") {
      toggleSwarm();
      e.preventDefault();
    }
    return;
  }
  const tag = (e.target && e.target.tagName) || "";
  if (tag === "INPUT" || tag === "TEXTAREA") return;
  const list = visibleAsks();
  if (list.length) {
    if (e.key === "j" || e.key === "ArrowDown") {
      cursor = Math.min(list.length - 1, cursor + 1);
      e.preventDefault();
      renderAll();
      return;
    }
    if (e.key === "k" || e.key === "ArrowUp") {
      cursor = Math.max(0, cursor - 1);
      e.preventDefault();
      renderAll();
      return;
    }
    if (answerKey(e.key)) {
      e.preventDefault();
      return;
    }
  }
  if (e.key === "s" || e.key === "S") {
    toggleSwarm();
    e.preventDefault();
    return;
  }
  if (e.key === "v" || e.key === "V") {
    toggleCliManager();
    e.preventDefault();
  }
}

function toggleAvailable() {
  available = !available;
  showToast(available ? "おかえりなさい。" : "離席します。群れは安全側で続行します。", "var(--ac)");
  renderAll();
}
function toggleSwarm() {
  showSwarm = !showSwarm;
  if (showSwarm) showCliManager = false;
  swarmSig = null;
  updateSwarm();
}

function toggleCliManager() {
  showCliManager = !showCliManager;
  if (showCliManager) {
    showSwarm = false;
    swarmSig = null;
    updateSwarm();
    if (agentClis.length === 0) fetchAgentClis();
  }
  updateCliManager();
}

async function fetchAgentClis({ preserveError = false } = {}) {
  cliLoading = true;
  if (!preserveError) cliManagerError = null;
  updateCliManager();
  try {
    agentClis = (await invoke("get_agent_clis")) || [];
  } catch (e) {
    cliManagerError = String(e);
  } finally {
    cliLoading = false;
    updateCliManager();
  }
}

async function updateAgentCli(cli) {
  if (!cli.canUpdate || cliUpdating !== null) return;
  cliUpdating = cli.id;
  cliManagerError = null;
  updateCliManager();
  try {
    const result = await invoke("update_agent_cli", { id: cli.id });
    agentClis = agentClis.map((item) => (item.id === cli.id ? result.cli : item));
    showToast(`${cli.label} を ${result.cli.version || "最新版"} に更新しました。`, "#4ec9a0");
  } catch (e) {
    const message = String(e);
    cliManagerError = message;
    showToast(`${cli.label} の更新に失敗しました。`, "var(--err)");
  } finally {
    cliUpdating = null;
    await fetchAgentClis({ preserveError: cliManagerError !== null });
  }
}

// ── notifications (blocking asks only) ───────────────────────────────────────
function maybeNotify(list) {
  const fresh = list.filter((a) => a.kind === "blocking" && !notified.has(a.askId));
  fresh.forEach((a) => notified.add(a.askId));
  for (const id of [...notified]) if (!list.some((a) => a.askId === id)) notified.delete(id);
  if (!fresh.length) return;
  try {
    if (typeof Notification === "undefined") return;
    const fire = () => {
      const title = fresh.length === 1 ? "判断をひとつ、お願いします" : `${fresh.length} 件の判断待ち`;
      new Notification(title, { body: fresh[0].prompt.slice(0, 120) });
    };
    if (Notification.permission === "granted") fire();
    else if (Notification.permission !== "denied") Notification.requestPermission().then((p) => p === "granted" && fire());
  } catch (e) {
    /* webview without notifications — the badge is the fallback */
  }
}

// ── data ─────────────────────────────────────────────────────────────────────
async function fetchAsks() {
  try {
    const a = (await invoke("get_pending_asks")) || [];
    for (const id of [...answered]) if (!a.some((x) => x.askId === id)) answered.delete(id);
    asks = a.filter((x) => !answered.has(x.askId));
    connected = true;
    maybeNotify(asks);
  } catch (e) {
    connected = false;
  }
  renderAll();
}
async function fetchSnapshot() {
  try {
    snapshot = (await invoke("get_snapshot")) || { live: [], recent: [] };
    connected = true;
  } catch (e) {
    connected = false;
  }
  renderAll();
}

function tickClock() {
  const d = new Date();
  const p = (n) => String(n).padStart(2, "0");
  document.getElementById("clock").textContent = `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
}

function boot() {
  document.getElementById("avail-toggle").addEventListener("click", toggleAvailable);
  document.getElementById("swarm-toggle").addEventListener("click", toggleSwarm);
  document.getElementById("cli-toggle").addEventListener("click", toggleCliManager);
  window.addEventListener("keydown", onKey);

  tickClock();
  setInterval(tickClock, 1000);

  fetchSnapshot();
  fetchAsks();
  setInterval(fetchAsks, 1500);
  setInterval(fetchSnapshot, 1500);

  // Push updates from the file watcher refresh the swarm without waiting for the poll.
  try {
    listen("snapshot", (ev) => {
      snapshot = ev.payload;
      connected = true;
      renderAll();
    });
  } catch (e) {
    /* polling still covers it */
  }
}

boot();
