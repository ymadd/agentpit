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
let showSettings = false;
let projectFilter = null;
let cursor = 0; // which pending ask is on stage
let connected = false;
let toastTimer = null;
let agentClis = [];
let cliLoading = false;
let cliUpdating = null;
let cliManagerError = null;
let cliLatest = {}; // id -> latest version string from the public registry (async, best-effort)

// settings (workflow tuning + role roster) — see settings_get/settings_save contract below.
let settingsLoading = false;
let settingsSaving = false;
let settingsError = null; // load error (settings_get rejected)
let settingsData = null; // last-fetched raw payload: { config_path, exists, workflow, roles, known_backends }
let settingsDraft = null; // editable working copy built from settingsData — see draftFromSettings()
let roleKeySeq = 0; // stable client-side key for role cards (name may be edited before save)

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
  if (!ts) return "up";
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
  updateSettings();
}

// Coalesce renderAll() calls from fetch/push sources into at most one per
// animation frame via a dirty flag + requestAnimationFrame. Interactive paths
// (answer, onKey j/k, toggleAvailable, …) call renderAll() directly for
// immediate feedback — they still land within the same frame budget.
let renderScheduled = false;
function scheduleRender() {
  if (renderScheduled) return;
  renderScheduled = true;
  requestAnimationFrame(() => {
    renderScheduled = false;
    renderAll();
  });
}

function updateStatusbar() {
  const counts = swarmCounts();
  const ml = document.getElementById("manager-line");
  if (!available) ml.textContent = "Away — the swarm continues on the safe side";
  else if (counts.projects === 0) ml.textContent = "The swarm is quiet";
  else ml.textContent = `One manager overseeing ${counts.projects} project(s)`;

  const list = visibleAsks();
  document.getElementById("pending-count").textContent = list.length;
  document.getElementById("pending-pill").classList.toggle("hidden", list.length === 0);

  const at = document.getElementById("avail-toggle");
  at.classList.toggle("away", !available);
  at.querySelector(".avail-label").textContent = available ? "Available" : "Away";
}

function updateFooter() {
  const counts = swarmCounts();
  document.getElementById("swarm-footer").textContent = `${counts.projects} projects · ${counts.total} agents`;
  const conn = document.getElementById("conn");
  conn.classList.toggle("live", connected);
  document.getElementById("conn-text").textContent = connected ? "Connected" : "Disconnected";
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
    available ? "Nothing is waiting on your decision." : "You are away. The swarm keeps going."
  );
  const sub = el(
    "p",
    "idle-sub",
    available
      ? `${counts.running} agents are working quietly across ${counts.projects} projects. When something only a human can decide comes up, exactly one thing appears here. Until then, you are free to step away.`
      : "Even while you are away, the swarm keeps moving on the safe side. Anything only a human can decide waits, and is reported together when you return."
  );
  wrap.append(orb, eyebrow, h, sub);

  if (available) {
    const chips = el("div", "chips");
    for (const t of ["O(1)", "one window", "never stalls"]) chips.appendChild(el("span", "chip", t));
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
  badge.append(el("span", "d"), el("span", "l", blocking ? "action" : "review"));
  hr.appendChild(badge);
  head.append(hl, hr);
  wrap.appendChild(head);

  // the card
  const card = el("div", "dec-card");
  card.appendChild(el("h2", "dec-title", a.prompt));
  card.appendChild(
    el("p", "dec-reason", blocking ? "A worker is stopped, waiting on your decision." : "There is a decision to confirm.")
  );
  const shortId = (a.askId || "").replace(/^ask-/, "").slice(0, 14);
  card.appendChild(el("div", "dec-context", `${shortId}  ·  proceeds on the safe side if no answer in ${a.timeoutSecs}s`));

  const actions = el("div", "dec-actions");
  const opts = a.options && a.options.length ? a.options : ["yes", "no"];
  const isYesNo = !(a.options && a.options.length);
  opts.forEach((opt, i) => {
    const btn = el("button", "dec-btn " + (i === 0 ? "approve" : "neutral"));
    btn.type = "button";
    const key = isYesNo ? (i === 0 ? "Y" : "N") : String(i + 1);
    const label = isYesNo ? (i === 0 ? "Yes" : "No") : opt;
    btn.append(el("span", "key", key), el("span", "lab", label));
    btn.addEventListener("click", () => answer(a.askId, opt));
    actions.appendChild(btn);
  });
  card.appendChild(actions);
  wrap.appendChild(card);

  // reassurance
  const foot = el("div", "dec-foot");
  let reassure = "Reversible work keeps going while you decide";
  if (list.length > 1) {
    const next = list[(cursor + 1) % list.length];
    const nextRun = runIndex()[next.runId];
    const nextProj = (nextRun && basename(nextRun.cwd)) || next.runId;
    reassure = `Reversible work continues · next: ${nextProj}`;
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
    sl.textContent = "done";
    sl.style.color = "#3f8f6f";
    sd.style.background = "#3f8f6f";
  } else if (m.status === "error" || m.status === "interrupted") {
    sl.textContent = "failed";
    sl.style.color = "var(--err)";
    sd.style.background = "var(--err)";
  } else {
    sl.textContent = m.status || "idle";
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
  title.appendChild(el("span", "t", "Swarm"));
  const filteredItems = projectFilter ? groups.get(projectFilter) || [] : null;
  const headCount = projectFilter
    ? `${filteredItems.filter((x) => x.m.status === "running").length} running / ${filteredItems.length} agents`
    : `${projects.length} projects · ${runningAll} running / ${totalAll} total`;
  title.appendChild(el("span", "c", headCount));
  ht.append(title, el("div", "swarm-sub", "You do not need to watch this — the manager is."));
  const close = el("button", "swarm-close");
  close.type = "button";
  close.append(el("span", "l", "Close"), el("span", "x", "✕"));
  close.addEventListener("click", toggleSwarm);
  head.append(ht, close);
  sheet.appendChild(head);

  // body: rail + main
  const body = el("div", "swarm-body");
  const rail = el("div", "swarm-rail");
  rail.appendChild(el("div", "rail-head", "PROJECTS"));
  const railList = el("div", "rail-list");
  railList.appendChild(railItem("All", null, totalAll, projectFilter === null));
  for (const p of projects) railList.appendChild(railItem(p, p, (groups.get(p) || []).length, projectFilter === p));
  rail.append(railList);
  body.appendChild(rail);

  const main = el("div", "swarm-main");
  const scroll = el("div", "swarm-scroll");
  const showProjects = projectFilter ? projects.filter((p) => p === projectFilter) : projects;
  if (showProjects.length === 0) scroll.appendChild(el("div", "swarm-empty", "No swarm is running right now."));
  for (const p of showProjects) {
    const items = groups.get(p) || [];
    const g = el("div", "swarm-group");
    const gh = el("div", "group-head");
    const gd = el("span", "dot");
    gd.style.background = projectColor(p);
    gh.append(
      gd,
      el("span", "name", p),
      el("span", "meta", `${items.filter((x) => x.m.status === "running").length} / ${items.length} running`)
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
// Public registry package per CLI id. Ids without an entry (e.g. antigravity, which
// ships no public npm package) have no upstream to compare against → unknown.
const CLI_NPM = {
  claude: "@anthropic-ai/claude-code",
  codex: "@openai/codex",
  gemini: "@google/gemini-cli",
  opencode: "opencode-ai",
};

// Pull the first semver core out of a raw `--version` line. Handles the real shapes:
//   '2.1.206 (Claude Code)'→2.1.206  'codex-cli 0.144.1'→0.144.1  '0.43.0'→0.43.0
//   '1.1.0'→1.1.0  '1.17.13'→1.17.13. Returns null when no semver is present.
function parseSemver(text) {
  if (!text) return null;
  const m = String(text).match(/\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?/);
  return m ? m[0] : null;
}
// Compare two extracted semvers by release core, prerelease ranked below its release.
function cmpSemver(a, b) {
  const [coreA, preA = ""] = a.split("-");
  const [coreB, preB = ""] = b.split("-");
  const na = coreA.split(".").map(Number);
  const nb = coreB.split(".").map(Number);
  for (let i = 0; i < 3; i++) {
    const d = (na[i] || 0) - (nb[i] || 0);
    if (d) return d < 0 ? -1 : 1;
  }
  if (preA === preB) return 0;
  if (!preA) return 1; // release beats prerelease of the same core
  if (!preB) return -1;
  return preA < preB ? -1 : 1;
}
// One of the three states rendered per row: update / current / unknown.
function cliVersionState(cli) {
  const installed = parseSemver(cli.version);
  const latest = parseSemver(cliLatest[cli.id]);
  let state = "unknown";
  if (installed && latest) state = cmpSemver(installed, latest) < 0 ? "update" : "current";
  return { installed, latest, state };
}

let cliSig = null;
function cliManagerSig() {
  return (
    `${cliLoading ? "L" : "-"}|${cliUpdating || "-"}|${cliManagerError || "-"}|` +
    agentClis
      .map((c) =>
        [
          c.id,
          c.label,
          c.installed ? 1 : 0,
          c.path || "",
          c.command || "",
          c.note || "",
          c.version || "",
          c.canUpdate ? 1 : 0,
          c.updateCommand || "",
          cliLatest[c.id] || "", // latestVersion — its arrival re-signs → fillCliDynamic only
        ].join("~")
      )
      .join("§")
  );
}

function updateCliManager() {
  const root = document.getElementById("cli-manager");
  if (!showCliManager) {
    root.classList.add("hidden");
    cliSig = null;
    return;
  }
  const sig = cliManagerSig();
  if (sig === cliSig) {
    root.classList.remove("hidden");
    return;
  }
  const firstOpen = cliSig === null;
  cliSig = sig;
  if (firstOpen) buildCliManager(root);
  else fillCliDynamic(root);
  root.classList.remove("hidden");
}

// Build the shell (scrim + panel + head) once so its entrance animation is not
// replayed on data changes; the dynamic content is filled by fillCliDynamic.
function buildCliManager(root) {
  root.innerHTML = "";
  const scrim = el("div", "cli-scrim");
  scrim.addEventListener("click", toggleCliManager);
  const panel = el("section", "cli-panel");
  panel.setAttribute("aria-label", "Agent CLI version management");

  const head = el("header", "cli-head");
  const intro = el("div", "cli-intro");
  intro.append(
    el("div", "cli-eyebrow", "TOOLCHAIN / LOCAL"),
    el("h2", "cli-title", "Agent CLI versions"),
    el("p", "cli-sub", "Check the CLIs agentpit actually calls, and update each with its own official updater.")
  );
  const headActions = el("div", "cli-head-actions");
  const refresh = el("button", "cli-refresh");
  refresh.type = "button";
  refresh.addEventListener("click", fetchAgentClis);
  const close = el("button", "cli-close", "✕");
  close.type = "button";
  close.setAttribute("aria-label", "Close");
  close.addEventListener("click", toggleCliManager);
  headActions.append(refresh, close);
  head.append(intro, headActions);
  panel.appendChild(head);

  panel.appendChild(el("div", "cli-list"));

  const foot = el("footer", "cli-foot");
  foot.append(
    el("span", "cli-summary"),
    el("span", "cli-safety", "Update commands are fixed. No arbitrary shell input is run.")
  );
  panel.appendChild(foot);
  root.append(scrim, panel);

  fillCliDynamic(root);
}

// Replace only the signature-dependent pieces, leaving scrim/panel/head intact.
function fillCliDynamic(root) {
  const refresh = root.querySelector(".cli-refresh");
  if (refresh) {
    refresh.textContent = cliLoading ? "Checking…" : "Recheck";
    refresh.disabled = cliLoading || cliUpdating !== null;
  }

  const list = root.querySelector(".cli-list");
  if (list) {
    list.innerHTML = "";
    if (cliManagerError) {
      const error = el("div", "cli-error");
      error.append(el("strong", null, "Update failed"), el("span", null, cliManagerError));
      list.appendChild(error);
    }
    if (cliLoading && agentClis.length === 0) {
      list.appendChild(el("div", "cli-empty", "Checking local CLIs…"));
    } else {
      for (const cli of agentClis) list.appendChild(cliRow(cli));
    }
  }

  const summary = root.querySelector(".cli-summary");
  if (summary) {
    const installed = agentClis.filter((cli) => cli.installed).length;
    summary.textContent = `${installed} / ${agentClis.length || 5} installed`;
  }
}

function cliRow(cli) {
  const meta = modelMeta(cli.id);
  const { installed: instVer, latest: latestVer, state: upState } = cliVersionState(cli);
  const highlight = cli.installed && upState === "update";
  const row = el("article", `cli-row${cli.installed ? "" : " missing"}${highlight ? " has-update" : ""}`);
  const mark = el("span", "cli-mark", meta.mono);
  mark.style.setProperty("--cli-color", meta.color);

  const identity = el("div", "cli-identity");
  const nameLine = el("div", "cli-name-line");
  nameLine.append(el("span", "cli-name", cli.label));
  const state = el("span", `cli-state ${cli.installed ? "ready" : "missing"}`, cli.installed ? "installed" : "missing");
  nameLine.appendChild(state);
  // Update-state badge — only meaningful for an installed CLI (missing already reads "missing").
  if (cli.installed) {
    const label = upState === "update" ? "update" : upState === "current" ? "current" : "unknown";
    nameLine.appendChild(el("span", `cli-upstate ${upState}`, label));
  }
  identity.append(nameLine, el("div", "cli-path mono", cli.path || `${cli.command} is not on PATH`));
  if (cli.note) identity.appendChild(el("div", "cli-note", cli.note));

  const version = el("div", `cli-version state-${upState}`);
  const instLine = el("div", "cli-vline");
  instLine.append(
    el("span", "cli-version-label", "INSTALLED"),
    el("strong", "mono", instVer || cli.version || "—")
  );
  const latestLine = el("div", "cli-vline latest");
  latestLine.append(el("span", "cli-version-label", "LATEST"), el("strong", "mono", latestVer || "—"));
  version.append(instLine, latestLine);

  const updating = cliUpdating === cli.id;
  const action = el("button", "cli-update", updating ? "Updating…" : "Update");
  action.type = "button";
  action.disabled = !cli.canUpdate || cliUpdating !== null;
  action.title = cli.canUpdate ? cli.updateCommand || "Update" : cli.note || "Cannot update";
  action.addEventListener("click", () => updateAgentCli(cli));

  row.append(mark, identity, version, action);
  return row;
}

// ══ Workflow Studio (settings) ═══════════════════════════════════════════════
// The gear opens a full-screen node-graph editor for the model-driven workflow.
//
// What PERSISTS to ~/.config/agentpit/config.toml (via settings_get/settings_save):
//   • the cast — [workflow.roles.*] (backend preference order + persona)
//   • the workflow knobs — [workflow] (manager_backend, max_depth, max_calls_per_manager,
//     use_mcp, enable_ask_human)
// held in `settingsDraft` (unchanged contract; see draftFromSettings). The Save button and the
// "unsaved/saved" indicator tracks THESE edits only.
//
// What is an ILLUSTRATIVE BLUEPRINT (never written to config): the canvas of steps/gensteps +
// the goal text. Steps visualize how a workflow unfolds and are a casting surface (drag a CLI or
// role onto a step), but the manager still IMPROVISES the real decomposition at runtime —
// "roles fix the cast, never the script". The blueprint is a local sketch, auto-saved to
// localStorage so it survives a reload; it does not drive execution.
//
// Contract (Tauri side):
//   invoke('settings_get') -> { config_path, exists,
//     workflow: { manager_backend, default_agents, max_depth, max_calls_per_manager,
//                 use_mcp, enable_ask_human },
//     roles: [{ name, backends, prompt }], known_backends: [...] }
//   invoke('settings_save', { payload: { workflow, roles } }) -> resolves/rejects(string)
//   invoke('get_agent_clis') -> [{ id, label, installed, path, command, note, version, ... }]

const ROLE_NAME_RE = /^[a-z0-9][a-z0-9_-]*$/;
const BLUEPRINT_KEY = "agentpit.studio.blueprint.v1";
// Wire/transport per backend — the CLI inventory has no transport field, so pin the known shapes
// (matches how each backend is actually launched: exec for the CLIs, ACP for opencode).
const CLI_TRANSPORT = { claude: "exec", codex: "exec", antigravity: "exec", gemini: "exec", opencode: "acp" };
const CLI_NAME = { claude: "Claude Code", codex: "Codex", antigravity: "Antigravity", gemini: "Gemini CLI", opencode: "OpenCode" };

let settingsDirty = false; // unsaved CONFIG edits (roles/workflow/types) — not the blueprint
let studio = null; // local studio/blueprint view state (see newStudio)
let studioBuilt = false; // shell mounted once
let typeKeySeq = 0; // stable client-side key for workflow-type cards (name may be edited)

function draftFromSettings(data) {
  const wf = data.workflow || {};
  return {
    known_backends: data.known_backends || [],
    reserved_type_names: data.reserved_type_names || ["new", "list"],
    workflow: {
      manager_backend: wf.manager_backend || "",
      default_agents: wf.default_agents || [],
      max_depth: wf.max_depth,
      max_calls_per_manager: wf.max_calls_per_manager,
      use_mcp: !!wf.use_mcp,
      enable_ask_human: !!wf.enable_ask_human,
    },
    roles: (data.roles || []).map((r) => ({
      _key: roleKeySeq++,
      name: r.name,
      backends: [...(r.backends || [])],
      prompt: r.prompt || "",
      model: r.model || "",
      isNew: false,
    })),
    // Named workflow presets ([workflow.types.*]). Per-type knob overrides are null = inherit base.
    types: (data.types || []).map((t) => ({
      _key: typeKeySeq++,
      name: t.name,
      title: t.title || "",
      prompt: t.prompt || "",
      roles: [...(t.roles || [])],
      manager_backend: t.manager_backend || "",
      max_depth: t.max_depth,
      max_calls_per_manager: t.max_calls_per_manager,
      use_mcp: t.use_mcp == null ? null : !!t.use_mcp,
      enable_ask_human: t.enable_ask_human == null ? null : !!t.enable_ask_human,
      isNew: false,
    })),
  };
}

// The currently-edited workflow: null = the base [workflow], else a type object from the draft.
function currentTypeObj() {
  if (!studio || studio.currentType == null || !settingsDraft) return null;
  return settingsDraft.types.find((t) => t._key === studio.currentType) || null;
}
function workflowTypeNames() {
  return settingsDraft ? settingsDraft.types.map((t) => t.name).filter(Boolean) : [];
}
// Worker roles (cast minus the reserved manager) — the pool a type selects from.
function workerRoleNames() {
  return (settingsDraft ? settingsDraft.roles : [])
    .map((r) => r.name)
    .filter((n) => n && n !== "manager");
}

// Mirrors the backend validation rules: ^[a-z0-9][a-z0-9_-]*$, no duplicate names.
function roleNameError(name, allRoles, selfKey) {
  if (!name) return "Enter a name";
  if (!ROLE_NAME_RE.test(name)) return "Only lowercase letters, digits, - and _ (must start alphanumeric)";
  if (allRoles.some((r) => r._key !== selfKey && r.name === name)) return "This name is already in use";
  return null;
}
function validateSettings(draft) {
  const errors = {};
  let ok = true;
  for (const r of draft.roles) {
    const msg = roleNameError(r.name, draft.roles, r._key);
    if (msg) {
      errors[r._key] = msg;
      ok = false;
    }
  }
  for (const t of draft.types || []) {
    const msg = typeNameError(t.name, t._key);
    if (msg) {
      errors["t" + t._key] = msg;
      ok = false;
    }
  }
  return { ok, errors };
}

// ── blueprint (local sketch) ─────────────────────────────────────────────────
// The canonical illustration of a model-driven run: diagnose → plan → implement → review →
// integrate, with review self-spawning an adversarial sub-swarm. Worker chips reference roles by
// NAME (resolved against the real cast) and CLIs by id; unknown names render as faint examples.
function seedBlueprint() {
  return {
    goal: { id: "goal", x: 40, y: 250, w: 210, text: '"Fix the auth flow"' },
    ghost: { id: "ghost", x: 1820, y: 236, w: 156 },
    steps: [
      { id: "s1", index: "01", name: "Diagnose", manager: "antigravity",
        persona: "Classify the task; pick the best fit from capability profiles.", behavior: "features→category(conf)→backend. LLM assist only on low confidence.",
        dynamic: false, ask: false, fanout: 1, workers: [{ type: "role", id: "longctx" }], x: 320, y: 200, w: 250 },
      { id: "s2", index: "02", name: "Plan", manager: "claude",
        persona: "Break the goal into ordered sub-tasks.", behavior: "No static DAG. Improvise on the spot and delegate to the right role.",
        dynamic: true, ask: false, fanout: 3, workers: [{ type: "role", id: "coder" }], x: 620, y: 200, w: 250 },
      { id: "s3", index: "03", name: "Implement", manager: "claude",
        persona: "Never stall on reversible work. Dispatch dynamically to the right role.", behavior: "Use rescue / ensemble / workflow as the situation calls for.",
        dynamic: true, ask: true, fanout: 4, workers: [{ type: "role", id: "coder" }, { type: "role", id: "refactorer" }], x: 920, y: 200, w: 250 },
      { id: "s4", index: "04", name: "Review", manager: "claude",
        persona: "Check spec violations, boundaries, and security.", behavior: "If unsure, summon a refutation swarm (self-spawns). Over-detection is penalized.",
        dynamic: true, ask: true, fanout: 3, spawns: true, workers: [{ type: "role", id: "reviewer" }, { type: "role", id: "security" }], x: 1220, y: 200, w: 250 },
      { id: "s5", index: "05", name: "Integrate", manager: "codex",
        persona: "Integrate findings and finalize the diff.", behavior: "Dedup overlaps. Ask only what only a human can decide.",
        dynamic: false, ask: true, fanout: 1, workers: [], x: 1520, y: 200, w: 250 },
    ],
    gensteps: [
      { id: "g1", name: "critique", role: "adversary", backend: "codex", x: 1180, y: 520, w: 180 },
      { id: "g2", name: "defense", role: "adversary", backend: "antigravity", x: 1390, y: 520, w: 180 },
      { id: "g3", name: "adjudication", role: "reviewer", backend: "claude", x: 1600, y: 520, w: 180 },
    ],
  };
}
// The blueprint (canvas sketch) is per-workflow: the base and each named type keep their own
// localStorage sketch, so switching workflows swaps the canvas. `base` is the default [workflow].
function currentWorkflowName() {
  const t = currentTypeObj();
  if (!t) return "base";
  // A freshly-added type has no name yet — key its sketch by _key so it doesn't share base's.
  return t.name ? t.name : "type-" + t._key;
}
function blueprintKey(name) {
  return `${BLUEPRINT_KEY}.${name}`;
}
function loadBlueprint(name) {
  try {
    const raw = localStorage.getItem(blueprintKey(name));
    if (!raw) return { data: seedBlueprint(), fresh: true };
    const p = JSON.parse(raw);
    if (!p || !Array.isArray(p.steps) || !p.goal) return { data: seedBlueprint(), fresh: true };
    const seed = seedBlueprint();
    return {
      data: {
        goal: p.goal || seed.goal,
        ghost: p.ghost || seed.ghost,
        steps: p.steps,
        gensteps: Array.isArray(p.gensteps) ? p.gensteps : [],
      },
      fresh: false,
    };
  } catch (e) {
    return { data: seedBlueprint(), fresh: true };
  }
}
function saveBlueprint() {
  if (!studio) return;
  try {
    localStorage.setItem(
      blueprintKey(currentWorkflowName()),
      JSON.stringify({ goal: studio.goal, ghost: studio.ghost, steps: studio.steps, gensteps: studio.gensteps })
    );
  } catch (e) {
    /* private mode / quota — the sketch is best-effort */
  }
}
// Load a workflow's blueprint into the live studio (used on open + on workflow switch).
function loadStudioBlueprint(name) {
  const { data, fresh } = loadBlueprint(name);
  studio.goal = data.goal;
  studio.ghost = data.ghost || { id: "ghost", x: 1820, y: 236, w: 156 };
  studio.steps = data.steps;
  studio.gensteps = data.gensteps;
  studio.seq = 0;
  studio.selectedId = null;
  if (fresh) relayout();
}

// ── studio state ─────────────────────────────────────────────────────────────
function newStudio() {
  const s = {
    selectedId: null,
    paletteOpen: true,
    zoom: 0.72,
    pan: { x: 40, y: 30 },
    drag: null, // palette drag in flight { kind, id, mono, color, label, x, y, active }
    hoverStepId: null,
    draggingNodeId: null,
    currentType: null, // null = base [workflow]; else a draft type _key
    goal: null,
    ghost: { id: "ghost", x: 1820, y: 236, w: 156 },
    steps: [],
    gensteps: [],
    seq: 0,
  };
  studio = s;
  loadStudioBlueprint("base");
  return s;
}
// Switch the edited workflow (null = base, else a type _key): persist the current sketch, load
// the target's, and reveal the workflow inspector.
function switchWorkflow(typeKey) {
  if (!studio) return;
  saveBlueprint();
  studio.currentType = typeKey;
  loadStudioBlueprint(currentWorkflowName());
  studio.selectedId = "goal";
  renderStudio();
  requestAnimationFrame(fitToView);
}

function cssVar(n) {
  return getComputedStyle(document.body).getPropertyValue(n).trim() || "";
}
function hexA(hex, a) {
  if (!hex || hex[0] !== "#") return hex;
  const h = hex.slice(1);
  const r = parseInt(h.slice(0, 2), 16), g = parseInt(h.slice(2, 4), 16), b = parseInt(h.slice(4, 6), 16);
  return `rgba(${r}, ${g}, ${b}, ${a})`;
}
function metaFor(id) {
  const m = modelMeta(id);
  return { mono: m.mono, color: m.color, label: CLI_NAME[id] || id };
}
// The palette CLI list = the real local inventory when known, else the 5 backends as placeholders.
function cliInventory() {
  if (agentClis && agentClis.length) {
    return agentClis.map((c) => ({
      id: c.id,
      label: c.label || CLI_NAME[c.id] || c.id,
      version: parseSemver(c.version) || c.version || "—",
      transport: CLI_TRANSPORT[c.id] || "exec",
      installed: !!c.installed,
      path: c.path || "",
      command: c.command || c.id,
      note: c.note || "",
    }));
  }
  const ids = (settingsDraft && settingsDraft.known_backends) || Object.keys(CLI_NAME);
  return ids.map((id) => ({
    id,
    label: CLI_NAME[id] || id,
    version: "—",
    transport: CLI_TRANSPORT[id] || "exec",
    installed: false,
    path: "",
    command: id,
    note: "",
  }));
}
function roleByName(name) {
  return (settingsDraft && settingsDraft.roles.find((r) => r.name === name)) || null;
}
function roleByKey(key) {
  return (settingsDraft && settingsDraft.roles.find((r) => r._key === key)) || null;
}
function castRoles() {
  return settingsDraft ? settingsDraft.roles : [];
}
// A step worker → chip data. role: resolve against the cast (unknown → faint example).
function resolveWorker(w) {
  if (w.type === "role") {
    const r = roleByName(w.id);
    if (!r) return { color: "#7d8595", mono: "", label: w.id, kind: "role", known: false };
    const primary = r.backends[0] ? metaFor(r.backends[0]) : { color: "#7d8595" };
    return { color: primary.color, mono: "", label: r.name || w.id, kind: "role", known: true };
  }
  const m = metaFor(w.id);
  return { color: m.color, mono: m.mono, label: m.label, kind: "cli", known: true };
}
function nodeH(n) {
  const t = typeof n === "string" ? n : n && (n.t || n.type);
  if (t === "goal") return 62;
  if (t === "genstep") return 84;
  if (t === "ghost") return 128;
  if (t === "step") {
    const wc = n && n.workers ? n.workers.length : 0;
    const extra = wc > 2 ? Math.ceil((wc - 2) / 2) * 26 : 0;
    return 222 + extra;
  }
  return 100;
}
function relayout() {
  const s = studio;
  s.steps.forEach((st, i) => { st.x = 320 + i * 300; st.y = 200; });
  const rev = s.steps.find((st) => st.spawns);
  if (rev) s.gensteps.forEach((g, i) => { g.x = rev.x - 40 + i * 210; g.y = 520; });
  const last = s.steps[s.steps.length - 1];
  s.ghost.x = (last ? last.x : 320) + 300;
  s.ghost.y = 236;
}
function fitToView() {
  const vp = document.querySelector("#settings .ws-canvas");
  if (!vp || !vp.clientWidth) return;
  const cw = vp.clientWidth, ch = vp.clientHeight;
  const leftInset = studio.paletteOpen ? 268 : 66;
  const rightInset = studio.selectedId ? 348 : 18;
  const all = [
    { ...studio.goal, t: "goal" },
    ...studio.steps.map((n) => ({ ...n, t: "step" })),
    ...studio.gensteps.map((n) => ({ ...n, t: "genstep" })),
    { ...studio.ghost, t: "ghost" },
  ];
  let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
  for (const n of all) {
    const h = nodeH(n);
    minX = Math.min(minX, n.x); minY = Math.min(minY, n.y);
    maxX = Math.max(maxX, n.x + n.w); maxY = Math.max(maxY, n.y + h);
  }
  const pad = 38;
  const availW = Math.max(120, cw - leftInset - rightInset - pad * 2);
  const availH = Math.max(120, ch - pad * 2);
  const bw = Math.max(1, maxX - minX), bh = Math.max(1, maxY - minY);
  const zoom = Math.min(1.05, Math.max(0.26, Math.min(availW / bw, availH / bh)));
  const panX = leftInset + pad + (availW - bw * zoom) / 2 - minX * zoom;
  const panY = pad + (availH - bh * zoom) / 2 - minY * zoom;
  studio.zoom = +zoom.toFixed(3);
  studio.pan = { x: Math.round(panX), y: Math.round(panY) };
  applyLayerTransform();
  renderTopbar();
}

// ── studio mutations ─────────────────────────────────────────────────────────
function markDirty() {
  settingsDirty = true;
  refreshSaveState();
}
function selectNode(id) {
  studio.selectedId = id;
  renderStudio();
}
function closeInspector() {
  studio.selectedId = null;
  renderStudio();
}
function togglePalette() {
  studio.paletteOpen = !studio.paletteOpen;
  renderStudio();
}

// blueprint edits (step workers / geometry / goal text) — auto-saved, NOT config-dirty
function addWorker(stepId, worker) {
  const st = studio.steps.find((s) => s.id === stepId);
  if (!st) return;
  st.workers = st.workers || [];
  if (st.workers.some((w) => w.type === worker.type && w.id === worker.id)) return;
  st.workers.push(worker);
  saveBlueprint();
  renderStudio();
}
function removeWorker(stepId, idx) {
  const st = studio.steps.find((s) => s.id === stepId);
  if (!st) return;
  (st.workers || []).splice(idx, 1);
  saveBlueprint();
  renderStudio();
}
function addStep() {
  const id = "st-" + ++studio.seq;
  const n = studio.steps.length + 1;
  const idx = n < 10 ? "0" + n : "" + n;
  studio.steps.push({ id, index: idx, name: "New step", manager: "claude", persona: "", behavior: "", dynamic: true, ask: false, fanout: 2, workers: [], x: 0, y: 200, w: 250 });
  relayout();
  studio.selectedId = id;
  saveBlueprint();
  renderStudio();
}
function deleteStep(id) {
  studio.steps = studio.steps.filter((s) => s.id !== id);
  studio.steps.forEach((s, i) => { const n = i + 1; s.index = n < 10 ? "0" + n : "" + n; });
  relayout();
  studio.selectedId = null;
  saveBlueprint();
  renderStudio();
}
function setStepField(id, key, val, render) {
  const s = studio.steps.find((x) => x.id === id);
  if (!s) return;
  s[key] = val;
  saveBlueprint();
  if (render) renderStudio();
}
function setGoalText(val, render) {
  studio.goal.text = val;
  saveBlueprint();
  if (render) renderStudio();
}

// config edits (cast + workflow knobs) — config-dirty
function addRole() {
  const key = roleKeySeq++;
  settingsDraft.roles.push({ _key: key, name: "", backends: [], prompt: "", model: "", isNew: true });
  studio.selectedId = "role:" + key;
  markDirty();
  renderStudio();
}
function deleteRole(key) {
  const r = roleByKey(key);
  settingsDraft.roles = settingsDraft.roles.filter((x) => x._key !== key);
  // strip the deleted role from any step's worker list (blueprint)
  if (r && r.name) {
    for (const st of studio.steps) st.workers = (st.workers || []).filter((w) => !(w.type === "role" && w.id === r.name));
    saveBlueprint();
  }
  studio.selectedId = null;
  markDirty();
  renderStudio();
}
function addBackendTo(key, cliId) {
  const r = roleByKey(key);
  if (!r || r.backends.includes(cliId)) return;
  r.backends.push(cliId);
  markDirty();
  renderStudio();
}
function removeBackendAt(key, idx) {
  const r = roleByKey(key);
  if (!r) return;
  r.backends.splice(idx, 1);
  markDirty();
  renderStudio();
}
function moveBackend(key, from, to) {
  const r = roleByKey(key);
  if (!r || to < 0 || to >= r.backends.length) return;
  const [x] = r.backends.splice(from, 1);
  r.backends.splice(to, 0, x);
  markDirty();
  renderStudio();
}
function setRoleField(key, field, val, render) {
  const r = roleByKey(key);
  if (!r) return;
  r[field] = val;
  markDirty();
  if (render) renderStudio();
  else if (field === "name") updateRoleNameErr(r);
}
function setWorkflowField(field, val) {
  settingsDraft.workflow[field] = val;
  markDirty();
}
function updateRoleNameErr(role) {
  const errEl = document.getElementById(`ws-role-err-${role._key}`);
  if (!errEl) return;
  const msg = roleNameError(role.name, settingsDraft.roles, role._key);
  errEl.textContent = msg || "";
  errEl.classList.toggle("hidden", !msg);
  refreshSaveState(); // a role name doesn't appear in the topbar; only save-enabled depends on it
}

// ── workflow types (config-dirty) ────────────────────────────────────────────
// Mirrors the settings.rs type rules: ^[a-z0-9][a-z0-9_-]*$, no duplicates, and no reserved
// name. The reserved set (`new`/`list`) is shipped in the payload (reserved_type_names) so this
// client hint can't drift from the settings.rs gate that actually rejects the save.
function typeNameError(name, selfKey) {
  if (!name) return "Enter a name";
  const reserved = (settingsDraft && settingsDraft.reserved_type_names) || ["new", "list"];
  if (reserved.includes(name)) return `'${name}' is reserved (used by the workflow generate/list commands)`;
  if (!ROLE_NAME_RE.test(name)) return "Only lowercase letters, digits, - and _ (must start alphanumeric)";
  if (settingsDraft.types.some((t) => t._key !== selfKey && t.name === name)) return "This name is already in use";
  return null;
}
function setTypeField(key, field, val, render) {
  const t = settingsDraft.types.find((x) => x._key === key);
  if (!t) return;
  t[field] = val;
  markDirty();
  if (render) renderStudio();
  else if (field === "name") updateTypeNameErr(t);
  else if (field === "title") renderTopbar(); // the workflow switcher shows title as the option label
}
function updateTypeNameErr(t) {
  const errEl = document.getElementById(`ws-type-err-${t._key}`);
  if (errEl) {
    const msg = typeNameError(t.name, t._key);
    errEl.textContent = msg || "";
    errEl.classList.toggle("hidden", !msg);
  }
  renderTopbar();
}
function toggleTypeRole(key, roleName, on) {
  const t = settingsDraft.types.find((x) => x._key === key);
  if (!t) return;
  const has = t.roles.includes(roleName);
  if (on && !has) t.roles.push(roleName);
  else if (!on && has) t.roles = t.roles.filter((r) => r !== roleName);
  markDirty();
}
function addType(seed) {
  const key = typeKeySeq++;
  const t = seed || {
    name: "", title: "", prompt: "", roles: [], manager_backend: "",
    max_depth: null, max_calls_per_manager: null, use_mcp: null, enable_ask_human: null,
  };
  t._key = key;
  if (t.isNew !== false) t.isNew = true;
  settingsDraft.types.push(t);
  markDirty();
  switchWorkflow(key); // reveals the new type's (empty) canvas + inspector
  return t;
}
function deleteType(key) {
  const t = settingsDraft.types.find((x) => x._key === key);
  settingsDraft.types = settingsDraft.types.filter((x) => x._key !== key);
  if (t && t.name) {
    try {
      localStorage.removeItem(blueprintKey(t.name));
    } catch (e) {
      /* best-effort */
    }
  }
  markDirty();
  switchWorkflow(null); // back to the base workflow
}

// ── ✨ workflow generation (Studio → CLI `workflow new --json` → editable draft) ──
let generating = false;
function openGenerateModal() {
  const root = document.getElementById("settings");
  if (root.querySelector(".ws-gen-modal")) return;
  const overlay = el("div", "ws-gen-modal");
  const card = el("div", "ws-gen-card");
  card.append(
    el("div", "ws-eyebrow", "GENERATE"),
    el("div", "ws-gen-title", "Generate a workflow"),
    el("p", "ws-gen-sub", "Describe the workflow you want. An agent drafts roles, personas, and steps as an editable draft.")
  );
  const ta = el("textarea", "ws-gen-input");
  ta.placeholder = "e.g. A workflow that strictly reviews PRs and hardens security & edge cases with refutation";
  card.appendChild(ta);
  const err = el("p", "ws-gen-err hidden");
  card.appendChild(err);
  const actions = el("div", "ws-gen-actions");
  const cancel = el("button", "ws-btn", "Cancel");
  cancel.type = "button";
  const go = el("button", "ws-save", "Generate");
  go.type = "button";
  const close = () => overlay.remove();
  cancel.addEventListener("click", close);
  overlay.addEventListener("click", (e) => {
    if (e.target === overlay) close();
  });
  go.addEventListener("click", async () => {
    const desc = ta.value.trim();
    if (!desc) {
      err.textContent = "Enter a description.";
      err.classList.remove("hidden");
      return;
    }
    if (generating) return;
    generating = true;
    err.classList.add("hidden");
    go.textContent = "Generating…";
    go.disabled = true;
    cancel.disabled = true;
    try {
      const proposal = await invoke("workflow_generate", { description: desc });
      applyProposal(proposal);
      close();
      showToast("Workflow generated. Review it, then save.", "#4ec9a0");
    } catch (e2) {
      err.textContent = String(e2);
      err.classList.remove("hidden");
    } finally {
      generating = false;
      go.textContent = "Generate";
      go.disabled = false;
      cancel.disabled = false;
    }
  });
  actions.append(cancel, go);
  card.appendChild(actions);
  overlay.appendChild(card);
  root.appendChild(overlay);
  ta.focus();
}
// Apply a generated proposal as an UNSAVED draft: merge its roles into the shared cast, add a new
// type, and seed the blueprint from the proposed steps. The user reviews, edits, then saves.
function applyProposal(p) {
  if (!p || !p.type) throw new Error("Invalid generation result.");
  // 1. Merge roles: add missing ones; only fill blanks on existing roles (keep hand edits).
  for (const r of p.roles || []) {
    if (!r || !r.name) continue;
    const existing = settingsDraft.roles.find((x) => x.name === r.name);
    if (existing) {
      if ((!existing.backends || !existing.backends.length) && Array.isArray(r.backends)) existing.backends = [...r.backends];
      if (!existing.prompt && r.prompt) existing.prompt = r.prompt;
    } else {
      settingsDraft.roles.push({ _key: roleKeySeq++, name: r.name, backends: [...(r.backends || [])], prompt: r.prompt || "", model: r.model || "", isNew: true });
    }
  }
  // 2. A uniquely-named type.
  let name = p.type;
  let n = 2;
  while (settingsDraft.types.some((t) => t.name === name)) name = `${p.type}-${n++}`;
  const t = {
    _key: typeKeySeq++, name, title: p.title || "", prompt: p.brief || "",
    roles: [...(p.uses_roles || [])],
    manager_backend: p.manager_backend || "",
    max_depth: p.max_depth == null ? null : p.max_depth,
    max_calls_per_manager: p.max_calls_per_manager == null ? null : p.max_calls_per_manager,
    use_mcp: p.use_mcp == null ? null : !!p.use_mcp,
    enable_ask_human: p.enable_ask_human == null ? null : !!p.enable_ask_human,
    isNew: true,
  };
  settingsDraft.types.push(t);
  // 3. Blueprint from the proposed steps (illustrative; falls back to the seed if none).
  studio.currentType = t._key;
  const steps = (p.steps || []).map((s, i) => ({
    id: "st-" + (i + 1),
    index: (i + 1 < 10 ? "0" : "") + (i + 1),
    name: s.name || `phase ${i + 1}`,
    manager: s.manager || p.manager_backend || "claude",
    persona: s.persona || "",
    behavior: s.behavior || "",
    dynamic: s.dynamic !== false,
    ask: !!s.ask,
    fanout: s.fanout || 2,
    workers: (s.workers || []).filter(Boolean).map((w) => ({ type: "role", id: w })),
    x: 0, y: 200, w: 250,
  }));
  studio.steps = steps.length ? steps : seedBlueprint().steps;
  studio.gensteps = [];
  studio.goal = { id: "goal", x: 40, y: 250, w: 210, text: "" };
  studio.seq = studio.steps.length;
  relayout();
  saveBlueprint();
  studio.selectedId = "goal";
  markDirty();
  renderStudio();
  requestAnimationFrame(fitToView);
}

// ── interactions ─────────────────────────────────────────────────────────────
function zoomBy(d) {
  studio.zoom = Math.min(1.4, Math.max(0.26, +(studio.zoom + d).toFixed(2)));
  applyLayerTransform();
  renderTopbar();
}
function beginPan(e) {
  if (e.button !== undefined && e.button !== 0) return;
  const canvas = document.querySelector("#settings .ws-canvas");
  const start = { mx: e.clientX, my: e.clientY, px: studio.pan.x, py: studio.pan.y };
  const hadSelection = studio.selectedId;
  studio.selectedId = null;
  if (hadSelection) renderStudio();
  if (canvas) canvas.classList.add("grabbing");
  const move = (ev) => {
    studio.pan = { x: start.px + (ev.clientX - start.mx), y: start.py + (ev.clientY - start.my) };
    applyLayerTransform();
  };
  const up = () => {
    document.removeEventListener("pointermove", move);
    document.removeEventListener("pointerup", up);
    if (canvas) canvas.classList.remove("grabbing");
  };
  document.addEventListener("pointermove", move);
  document.addEventListener("pointerup", up);
}
function beginNodeDrag(id, e) {
  if (e.button !== undefined && e.button !== 0) return;
  e.stopPropagation();
  const n = studio.steps.find((s) => s.id === id);
  if (!n) return;
  const z = studio.zoom;
  const start = { mx: e.clientX, my: e.clientY, nx: n.x, ny: n.y };
  studio.draggingNodeId = id;
  studio.selectedId = id;
  renderStudio();
  const nodeEl = document.querySelector(`#settings .ws-node[data-node="${id}"]`);
  const move = (ev) => {
    const dx = (ev.clientX - start.mx) / z, dy = (ev.clientY - start.my) / z;
    n.x = Math.round(start.nx + dx); n.y = Math.round(start.ny + dy);
    if (nodeEl) nodeEl.style.transform = `translate(${n.x}px, ${n.y}px)`;
    redrawEdges();
  };
  const up = () => {
    document.removeEventListener("pointermove", move);
    document.removeEventListener("pointerup", up);
    studio.draggingNodeId = null;
    saveBlueprint();
  };
  document.addEventListener("pointermove", move);
  document.addEventListener("pointerup", up);
}
// Palette drag: threshold distinguishes a click (→ select) from a drag (→ assign to a step).
function beginPaletteDrag(kind, id, e) {
  if (e.button !== undefined && e.button !== 0) return;
  e.preventDefault();
  e.stopPropagation();
  let mono = "", color = "#7d8595", label = id;
  if (kind === "cli") { const m = metaFor(id); mono = m.mono; color = m.color; label = m.label; }
  else { const r = roleByName(id); const p = r && r.backends[0] ? metaFor(r.backends[0]) : { color: "#7d8595" }; color = p.color; label = id; }
  const start = { x: e.clientX, y: e.clientY };
  let active = false;
  const ghost = document.querySelector("#settings .ws-ghostdrag");
  const move = (ev) => {
    if (!active) {
      if (Math.hypot(ev.clientX - start.x, ev.clientY - start.y) < 4) return;
      active = true;
      studio.drag = { kind, id, mono, color, label };
      if (ghost) {
        ghost.querySelector(".ws-mono").textContent = mono;
        ghost.querySelector(".ws-mono").style.background = color;
        ghost.querySelector(".nm").textContent = label;
        ghost.classList.remove("hidden");
      }
    }
    if (ghost) { ghost.style.left = ev.clientX + 12 + "px"; ghost.style.top = ev.clientY + 12 + "px"; }
    const t = document.elementFromPoint(ev.clientX, ev.clientY);
    const drop = t && t.closest && t.closest("[data-step-drop]");
    const hover = drop ? drop.getAttribute("data-step-drop") : null;
    if (hover !== studio.hoverStepId) {
      studio.hoverStepId = hover;
      markStepHover(hover);
    }
  };
  const up = () => {
    document.removeEventListener("pointermove", move);
    document.removeEventListener("pointerup", up);
    if (ghost) ghost.classList.add("hidden");
    const hover = studio.hoverStepId;
    studio.drag = null;
    studio.hoverStepId = null;
    if (!active) { selectNode(kind + ":" + (kind === "role" ? (roleByName(id) || {})._key : id)); return; }
    if (hover) addWorker(hover, { type: kind, id });
    else renderStudio();
  };
  document.addEventListener("pointermove", move);
  document.addEventListener("pointerup", up);
}
function markStepHover(stepId) {
  document.querySelectorAll("#settings .ws-step").forEach((el2) => {
    const on = el2.getAttribute("data-step-drop") === stepId;
    el2.classList.toggle("hover", on);
    const workers = el2.querySelector(".ws-workers");
    if (workers) workers.classList.toggle("drop", on);
  });
}

// ── rendering ────────────────────────────────────────────────────────────────
function buildStudioShell() {
  if (studioBuilt) return;
  const root = document.getElementById("settings");
  root.innerHTML = "";

  const top = el("div", "ws-top");
  top.innerHTML = "";
  root.appendChild(top);

  const body = el("div", "ws-body");
  const canvas = el("div", "ws-canvas");
  canvas.setAttribute("data-ap-canvas", "1");
  const layer = el("div", "ws-layer");
  canvas.appendChild(layer);
  const hint = el("div", "ws-hint");
  hint.append(el("span", null, "drag = pan"), el("span", null, "header = move"), el("span", null, "drop a CLI/role on a step"));
  canvas.appendChild(hint);
  canvas.addEventListener("pointerdown", (e) => { if (e.target === canvas || e.target === layer) beginPan(e); });
  body.appendChild(canvas);
  body.appendChild(el("div", "ws-pal-holder"));
  body.appendChild(el("div", "ws-insp-holder"));
  root.appendChild(body);

  root.appendChild(el("div", "ws-status"));

  const ghost = el("div", "ws-ghostdrag hidden");
  const inner = el("div", "inner");
  inner.append(el("span", "ws-mono"), el("span", "nm"));
  ghost.appendChild(inner);
  root.appendChild(ghost);

  studioBuilt = true;
}

function renderStudio() {
  if (!studioBuilt || !studio) return;
  renderTopbar();
  renderCanvas();
  renderPalette();
  renderInspector();
  renderStatus();
}

function renderTopbar() {
  const top = document.querySelector("#settings .ws-top");
  if (!top) return;
  top.innerHTML = "";
  const left = el("div", "ws-top-l");
  const live = el("span", "ws-live");
  live.append(el("span", "core"), el("span", "halo"));
  left.append(live, el("span", "ws-brand", "agentpit"), el("span", "ws-crumb", "Settings /"));

  // Workflow switcher: base [workflow] + each named type + "+ new". Changing it swaps the canvas.
  const sw = el("select", "ws-switch");
  const base = el("option", null, "(default) workflow"); base.value = "base"; sw.appendChild(base);
  for (const t of settingsDraft ? settingsDraft.types : []) {
    const o = el("option", null, t.title || t.name || "(unnamed)"); o.value = "t" + t._key; sw.appendChild(o);
  }
  const addOpt = el("option", null, "+ New workflow"); addOpt.value = "__new"; sw.appendChild(addOpt);
  sw.value = studio.currentType == null ? "base" : "t" + studio.currentType;
  sw.addEventListener("change", () => {
    if (sw.value === "__new") { addType(); return; }
    switchWorkflow(sw.value === "base" ? null : parseInt(sw.value.slice(1), 10));
  });
  left.appendChild(sw);

  const badge = el("span", "ws-badge", "BLUEPRINT");
  badge.title = "The canvas is a blueprint the runtime grows. Steps are not saved to config (only the cast and workflow settings are).";
  left.appendChild(badge);

  const right = el("div", "ws-top-r");
  const gen = el("button", "ws-gen-btn", "✨ Generate"); gen.type = "button";
  gen.title = "Generate a workflow from a description";
  gen.addEventListener("click", openGenerateModal);
  right.appendChild(gen);
  const nSteps = studio.steps.length;
  right.appendChild(el("span", "ws-grow", `grows to ${nSteps}–N steps at runtime`));
  const zoom = el("div", "ws-zoom");
  const zo = el("button", null, "−"); zo.type = "button"; zo.addEventListener("click", () => zoomBy(-0.1));
  const zp = el("span", "pct", Math.round(studio.zoom * 100) + "%");
  const zi = el("button", null, "+"); zi.type = "button"; zi.addEventListener("click", () => zoomBy(0.1));
  zoom.append(zo, zp, zi);
  right.appendChild(zoom);
  const fit = el("button", "ws-btn", "Fit"); fit.type = "button"; fit.addEventListener("click", fitToView);
  right.appendChild(fit);
  right.appendChild(el("span", "ws-div"));

  // The saved-indicator text and Save-disabled state are dynamic (change on every edit); build the
  // shells here and let refreshSaveState() fill them, so that logic lives in exactly one place and
  // a value-only keystroke can update it via markDirty() without rebuilding the whole bar.
  const saved = el("span", "ws-saved");
  right.appendChild(saved);
  const save = el("button", "ws-save", "Save"); save.type = "button";
  save.addEventListener("click", saveSettings);
  right.appendChild(save);

  const close = el("button", "ws-close", "✕"); close.type = "button";
  close.setAttribute("aria-label", "Close");
  close.addEventListener("click", toggleSettings);
  right.appendChild(close);

  top.append(left, right);
  refreshSaveState();
}

// Update only the two topbar elements a value edit can change — the saved indicator and the Save
// button — instead of tearing down and rebuilding the whole bar (select, buttons, listeners) on
// each keystroke. Structural changes (add/delete/switch/rename a type) still call renderTopbar().
function refreshSaveState() {
  const top = document.querySelector("#settings .ws-top");
  if (!top) return;
  const saved = top.querySelector(".ws-saved");
  const save = top.querySelector(".ws-save");
  if (!saved || !save) return;
  const { ok } = validateSettings(settingsDraft || { roles: [] });
  let savedText;
  saved.classList.remove("dirty");
  if (settingsSaving) savedText = "Saving…";
  else if (settingsError && !settingsDraft) savedText = "Load failed";
  else if (settingsDirty) { saved.classList.add("dirty"); savedText = "Unsaved changes"; }
  else savedText = settingsData && settingsData.exists === false ? "not created" : "Saved";
  saved.replaceChildren(el("span", "dot"), el("span", null, savedText));
  save.disabled = !settingsDraft || !ok || !settingsDirty || settingsSaving;
}

function applyLayerTransform() {
  const layer = document.querySelector("#settings .ws-layer");
  const canvas = document.querySelector("#settings .ws-canvas");
  if (!layer || !canvas) return;
  layer.style.transform = `translate(${studio.pan.x}px, ${studio.pan.y}px) scale(${studio.zoom})`;
  const gs = 22 * studio.zoom;
  canvas.style.backgroundSize = `${gs}px ${gs}px`;
  canvas.style.backgroundPosition = `${studio.pan.x}px ${studio.pan.y}px`;
}

function nodeById(id) {
  if (id === studio.goal.id) return { ...studio.goal, t: "goal" };
  const st = studio.steps.find((s) => s.id === id);
  if (st) return { ...st, t: "step" };
  const g = studio.gensteps.find((s) => s.id === id);
  if (g) return { ...g, t: "genstep" };
  if (id === studio.ghost.id) return { ...studio.ghost, t: "ghost" };
  return null;
}
function port(n, side) {
  const h = nodeH(n);
  if (side === "r") return [n.x + n.w, n.y + h / 2];
  if (side === "l") return [n.x, n.y + h / 2];
  if (side === "t") return [n.x + n.w / 2, n.y];
  return [n.x + n.w / 2, n.y + h];
}
function ctrl(px, py, side, k) {
  return side === "r" ? [px + k, py] : side === "l" ? [px - k, py] : side === "t" ? [px, py - k] : [px, py + k];
}
function edgeDefs() {
  const s = studio;
  const defs = [];
  if (s.steps.length) defs.push({ from: "goal", to: s.steps[0].id, kind: "flow", fs: "r", ts: "l", label: "goal in" });
  for (let i = 0; i < s.steps.length - 1; i++) defs.push({ from: s.steps[i].id, to: s.steps[i + 1].id, kind: "handoff", fs: "r", ts: "l", label: "" });
  const rev = s.steps.find((x) => x.spawns);
  if (rev && s.gensteps.length) {
    defs.push({ from: rev.id, to: s.gensteps[0].id, kind: "spawn", fs: "b", ts: "t", label: "auto-spawn" });
    for (let i = 0; i < s.gensteps.length - 1; i++) defs.push({ from: s.gensteps[i].id, to: s.gensteps[i + 1].id, kind: "subflow", fs: "r", ts: "l", label: "" });
    const ri = s.steps.indexOf(rev); const nxt = s.steps[ri + 1];
    if (nxt) defs.push({ from: s.gensteps[s.gensteps.length - 1].id, to: nxt.id, kind: "return", fs: "t", ts: "b", label: "integrate" });
  }
  if (s.steps.length) defs.push({ from: s.steps[s.steps.length - 1].id, to: "ghost", kind: "grow", fs: "r", ts: "l", label: "dynamic" });
  return defs;
}
function svgMarkup() {
  const acMain = cssVar("--ac") || "#e0926e";
  const acTx = cssVar("--ac-tx") || "#f0b59a";
  const styleFor = (kind) => {
    if (kind === "flow") return { stroke: "#7c8595", width: 1.8, dash: "0", marker: "ah-mut", anim: "", color: "#7c8595" };
    if (kind === "handoff") return { stroke: "#8089a0", width: 2, dash: "0", marker: "ah-mut", anim: "", color: "#8089a0" };
    if (kind === "spawn") return { stroke: acMain, width: 1.8, dash: "5 6", marker: "ah-ac", anim: "animation: ap-dash 1s linear infinite;", color: acTx };
    if (kind === "subflow") return { stroke: acMain, width: 1.6, dash: "5 6", marker: "ah-ac", anim: "animation: ap-dash 1s linear infinite;", color: acTx };
    if (kind === "return") return { stroke: acTx, width: 1.5, dash: "2 6", marker: "ah-ac", anim: "", color: acTx };
    return { stroke: "#4c5667", width: 1.6, dash: "4 6", marker: "ah-mut", anim: "", color: "#4c5667" };
  };
  let paths = `<defs><marker id="ah-ac" markerWidth="8" markerHeight="8" refX="6" refY="3" orient="auto"><path d="M0 0 L6 3 L0 6 Z" fill="${acMain}"></path></marker>` +
    `<marker id="ah-mut" markerWidth="8" markerHeight="8" refX="6" refY="3" orient="auto"><path d="M0 0 L6 3 L0 6 Z" fill="#7c8595"></path></marker></defs>`;
  const labels = [];
  for (const e of edgeDefs()) {
    const a = nodeById(e.from), b = nodeById(e.to);
    if (!a || !b) continue;
    const [sx, sy] = port(a, e.fs), [tx, ty] = port(b, e.ts);
    const k = Math.max(38, Math.hypot(tx - sx, ty - sy) * 0.45);
    const [c1x, c1y] = ctrl(sx, sy, e.fs, k), [c2x, c2y] = ctrl(tx, ty, e.ts, k);
    const st = styleFor(e.kind);
    paths += `<path d="M ${sx} ${sy} C ${c1x} ${c1y}, ${c2x} ${c2y}, ${tx} ${ty}" fill="none" stroke="${st.stroke}" stroke-width="${st.width}" stroke-linecap="round" stroke-dasharray="${st.dash}" marker-end="url(#${st.marker})" style="${st.anim}"></path>`;
    if (e.label) labels.push({ x: (sx + tx) / 2, y: (sy + ty) / 2, label: e.label, color: st.color });
  }
  return { paths, labels };
}
function redrawEdges() {
  const svg = document.querySelector("#settings .ws-svg");
  if (!svg) return;
  const { paths, labels } = svgMarkup();
  svg.innerHTML = paths;
  const layer = svg.parentElement;
  layer.querySelectorAll(".ws-elabel").forEach((n) => n.remove());
  for (const l of labels) {
    const d = el("div", "ws-elabel", l.label);
    d.style.left = l.x + "px"; d.style.top = l.y + "px"; d.style.color = l.color;
    layer.appendChild(d);
  }
}

function mono(text, color, cls) {
  const s = el("span", "ws-mono" + (cls ? " " + cls : ""), text);
  s.style.background = color;
  return s;
}
function renderCanvas() {
  const layer = document.querySelector("#settings .ws-layer");
  if (!layer) return;
  layer.innerHTML = "";
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("class", "ws-svg");
  svg.setAttribute("width", "3200");
  svg.setAttribute("height", "1600");
  layer.appendChild(svg);
  // nodes
  layer.appendChild(buildGoalNode());
  for (const st of studio.steps) layer.appendChild(buildStepNode(st));
  for (const g of studio.gensteps) layer.appendChild(buildGenNode(g));
  layer.appendChild(buildGhostNode());
  redrawEdges();
  applyLayerTransform();
}
function nodeWrap(id, x, y, w, selected) {
  const n = el("div", "ws-node");
  n.setAttribute("data-node", id);
  n.style.width = w + "px";
  n.style.transform = `translate(${x}px, ${y}px)`;
  n.style.boxShadow = selected ? "0 0 0 1.5px var(--ac), 0 20px 46px -20px rgba(0,0,0,.72)" : "0 16px 38px -22px rgba(0,0,0,.7)";
  return n;
}
function buildGoalNode() {
  const g = studio.goal;
  const wrap = nodeWrap("goal", g.x, g.y, g.w, studio.selectedId === "goal");
  const card = el("div", "ws-goal");
  // The root node is the WORKFLOW itself (base or a named type), not a goal input.
  const t = currentTypeObj();
  const label = t ? t.name || "(unnamed)" : "(default)";
  card.append(el("div", "eyebrow", "WORKFLOW"), el("div", "txt", label));
  card.append(portEl("r"));
  card.addEventListener("click", (e) => { e.stopPropagation(); selectNode("goal"); });
  card.addEventListener("pointerdown", (e) => e.stopPropagation());
  wrap.appendChild(card);
  return wrap;
}
function portEl(side, ac) {
  return el("span", "ws-port " + side + (ac ? " ac" : ""));
}
function buildStepNode(st) {
  const selected = studio.selectedId === st.id;
  const wrap = nodeWrap(st.id, st.x, st.y, st.w, selected);
  const card = el("div", "ws-step" + (selected ? " sel" : ""));
  card.setAttribute("data-step-drop", st.id);
  card.addEventListener("click", (e) => { e.stopPropagation(); selectNode(st.id); });
  card.addEventListener("pointerdown", (e) => e.stopPropagation());

  const hd = el("div", "ws-step-hd");
  hd.addEventListener("pointerdown", (e) => beginNodeDrag(st.id, e));
  hd.append(el("span", "ws-idx", st.index), el("span", "ws-step-name", st.name));
  const dyn = el("span", "ws-dyn " + (st.dynamic ? "on" : "off"), st.dynamic ? "⟳ self-spawn" : "fixed");
  hd.appendChild(dyn);
  card.appendChild(hd);

  const bd = el("div", "ws-step-bd");
  const mm = metaFor(st.manager);
  const mgr = el("div", "ws-mgr");
  mgr.append(mono(mm.mono, mm.color), el("span", "ws-mgr-tag", "manager ·"), el("span", "ws-mgr-label", mm.label));
  const ask = el("span", "ws-ask", st.ask ? "ask ✓" : "");
  ask.style.color = st.ask ? "var(--ac-tx)" : "var(--faint)";
  mgr.appendChild(ask);
  bd.appendChild(mgr);
  if (st.persona) bd.appendChild(el("div", "ws-persona", st.persona));
  const dir = el("div", "ws-directive");
  dir.append(el("div", "lb", "BEHAVIOR / DIRECTIVE"), el("div", "bd", st.behavior || "—"));
  bd.appendChild(dir);

  const workers = el("div", "ws-workers");
  workers.appendChild(el("span", "ws-worklabel", "worker"));
  (st.workers || []).forEach((w, i) => {
    const rw = resolveWorker(w);
    const chip = el("span", "ws-chip" + (rw.known ? "" : " unknown"));
    if (rw.known) chip.style.background = hexA(rw.color, 0.14), chip.style.border = "1px solid " + hexA(rw.color, 0.5);
    if (rw.mono) chip.appendChild(mono(rw.mono, rw.color));
    else { const sw = el("span"); sw.style.cssText = `width:10px;height:10px;border-radius:3px;flex:none;background:${rw.color};`; chip.appendChild(sw); }
    chip.appendChild(el("span", "nm", rw.label));
    const x = el("span", "x", "✕");
    x.addEventListener("click", (e) => { e.stopPropagation(); removeWorker(st.id, i); });
    x.addEventListener("pointerdown", (e) => e.stopPropagation());
    chip.appendChild(x);
    workers.appendChild(chip);
  });
  if (!(st.workers || []).length) workers.appendChild(el("span", "ws-drophint", "drop a CLI/role"));
  bd.appendChild(workers);
  card.appendChild(bd);

  card.append(portEl("l"), portEl("r"));
  if (st.spawns) card.appendChild(portEl("b", true));
  wrap.appendChild(card);
  return wrap;
}
function buildGenNode(g) {
  const wrap = nodeWrap(g.id, g.x, g.y, g.w, studio.selectedId === g.id);
  const card = el("div", "ws-gen");
  const hd = el("div", "ws-gen-hd");
  hd.append(el("span", "g", "GENERATED"), el("span", "r", "runtime"));
  card.appendChild(hd);
  const mm = metaFor(g.backend);
  const bd = el("div", "ws-gen-bd");
  bd.append(mono(mm.mono, mm.color), el("span", "nm", g.name));
  card.appendChild(bd);
  card.append(portEl("t", true), portEl("l", true), portEl("r", true));
  card.addEventListener("click", (e) => { e.stopPropagation(); selectNode(g.id); });
  card.addEventListener("pointerdown", (e) => e.stopPropagation());
  wrap.appendChild(card);
  return wrap;
}
function buildGhostNode() {
  const gh = studio.ghost;
  const wrap = nodeWrap("ghost", gh.x, gh.y, gh.w, false);
  wrap.style.boxShadow = "none";
  const btn = el("button", "ws-ghost");
  btn.type = "button";
  btn.append(el("div", "g", "DYNAMIC"), el("div", "plus", "+"));
  const cap = el("div", "cap"); cap.innerHTML = "spawns at runtime<br />(add manually too)";
  btn.appendChild(cap);
  btn.appendChild(portEl("l"));
  btn.addEventListener("click", (e) => { e.stopPropagation(); addStep(); });
  btn.addEventListener("pointerdown", (e) => e.stopPropagation());
  wrap.appendChild(btn);
  return wrap;
}

function renderPalette() {
  const holder = document.querySelector("#settings .ws-pal-holder");
  if (!holder) return;
  holder.innerHTML = "";
  if (!studio.paletteOpen) {
    const rail = el("div", "ws-rail");
    const btn = el("button", "ws-collapse", "»"); btn.type = "button"; btn.addEventListener("click", togglePalette);
    rail.appendChild(btn);
    for (const c of cliInventory()) {
      const meta = metaFor(c.id);
      const m = mono(meta.mono, meta.color);
      m.title = c.label;
      m.addEventListener("pointerdown", (e) => beginPaletteDrag("cli", c.id, e));
      rail.appendChild(m);
    }
    holder.appendChild(rail);
    return;
  }
  const pal = el("div", "ws-palette");
  const hd = el("div", "ws-pal-hd");
  const row = el("div", "ws-pal-hd-row");
  const col = el("div");
  col.append(el("div", "ws-eyebrow", "LIBRARY"), el("div", "ws-pal-title", "Agents & cast"));
  const collapse = el("button", "ws-collapse", "«"); collapse.type = "button"; collapse.addEventListener("click", togglePalette);
  row.append(col, collapse);
  hd.appendChild(row);
  hd.appendChild(el("div", "ws-pal-hint", "Drag onto a step to assign a worker. The cast and settings save to config."));
  pal.appendChild(hd);

  const list = el("div", "ws-pal-list");
  const sec1 = el("div", "ws-pal-sec first"); sec1.appendChild(el("span", null, "AGENT CLI"));
  list.appendChild(sec1);
  for (const c of cliInventory()) list.appendChild(paletteCli(c));

  const sec2 = el("div", "ws-pal-sec");
  sec2.appendChild(el("span", null, "Roles / cast"));
  const add = el("button", "ws-pal-add", "+"); add.type = "button"; add.title = "Add a role"; add.addEventListener("click", addRole);
  sec2.appendChild(add);
  list.appendChild(sec2);
  const roles = castRoles();
  if (!roles.length) list.appendChild(el("div", "ws-pal-hint", "No roles yet. Add one with +."));
  for (const r of roles) list.appendChild(paletteRole(r));
  pal.appendChild(list);

  const foot = el("div", "ws-pal-foot");
  const addStepBtn = el("button", "ws-pal-addstep", "+ Add step"); addStepBtn.type = "button"; addStepBtn.addEventListener("click", addStep);
  foot.appendChild(addStepBtn);
  pal.appendChild(foot);
  holder.appendChild(pal);
}
function paletteCli(c) {
  const m = metaFor(c.id);
  const item = el("div", "ws-pal-item" + (studio.selectedId === "cli:" + c.id ? " sel" : ""));
  item.appendChild(mono(m.mono, m.color));
  const col = el("div", "col");
  col.appendChild(el("div", "nm", c.label));
  col.appendChild(el("div", "sub" + (c.installed ? "" : " miss"), `${c.version} · ${c.transport}${c.installed ? "" : " · missing"}`));
  item.appendChild(col);
  item.addEventListener("pointerdown", (e) => beginPaletteDrag("cli", c.id, e));
  item.addEventListener("click", () => selectNode("cli:" + c.id));
  return item;
}
function paletteRole(r) {
  const primary = r.backends[0] ? metaFor(r.backends[0]) : { color: "#7d8595" };
  const item = el("div", "ws-pal-item" + (studio.selectedId === "role:" + r._key ? " sel" : ""));
  const sw = el("span", "swatch"); sw.style.background = primary.color;
  item.appendChild(sw);
  const col = el("div", "col");
  col.appendChild(el("div", "nm", r.name || "(unnamed)"));
  col.appendChild(el("div", "sub", (r.backends || []).join(" › ") || "—"));
  item.appendChild(col);
  if (r.name === "manager") { const b = el("span", "ws-badge", "MGR"); item.appendChild(b); }
  item.addEventListener("pointerdown", (e) => { if (r.name) beginPaletteDrag("role", r.name, e); else selectNode("role:" + r._key); });
  item.addEventListener("click", () => selectNode("role:" + r._key));
  return item;
}

function renderStatus() {
  const bar = document.querySelector("#settings .ws-status");
  if (!bar) return;
  bar.innerHTML = "";
  const legend = el("div", "ws-legend");
  const mk = (style, label) => { const s = el("span"); const i = el("i"); i.style.cssText = style; s.append(i, document.createTextNode(label)); return s; };
  legend.append(
    mk("border-top:1.8px solid var(--muted);", "handoff"),
    mk("border-top:1.8px dashed var(--ac);", "auto-spawn"),
    mk("border-top:1.8px dashed var(--ac-tx);", "integrate")
  );
  bar.appendChild(legend);
  const nRoles = castRoles().length;
  bar.appendChild(el("span", "ws-counts", `${studio.steps.length} steps · ${nRoles} roles`));
}

// ── inspector (5 forms) ──────────────────────────────────────────────────────
function renderInspector() {
  const holder = document.querySelector("#settings .ws-insp-holder");
  if (!holder) return;
  holder.innerHTML = "";
  const sel = studio.selectedId;
  if (!sel) return;

  const insp = el("div", "ws-insp");
  const hd = el("div", "ws-insp-hd");
  const col = el("div", "col");
  const eyebrow = el("div", "ws-eyebrow", "INSPECTOR");
  const title = el("div", "ws-insp-title");
  const sub = el("div", "ws-insp-sub");
  col.append(eyebrow, title, sub);
  const x = el("button", "ws-insp-x", "✕"); x.type = "button"; x.addEventListener("click", closeInspector);
  hd.append(col, x);
  insp.appendChild(hd);

  const bd = el("div", "ws-insp-bd");
  let meta = { title: "Nothing selected", sub: "" };
  if (sel === "goal") meta = inspGoal(bd);
  else if (sel.indexOf("cli:") === 0) meta = inspCli(bd, sel.slice(4));
  else if (sel.indexOf("role:") === 0) meta = inspRole(bd, parseInt(sel.slice(5), 10));
  else {
    const st = studio.steps.find((s) => s.id === sel);
    if (st) meta = inspStep(bd, st);
    else { const g = studio.gensteps.find((s) => s.id === sel); if (g) meta = inspGen(bd, g); }
  }
  title.textContent = meta.title;
  sub.textContent = meta.sub;
  insp.appendChild(bd);

  const foot = el("div", "ws-insp-foot");
  foot.textContent = settingsData && settingsData.config_path
    ? settingsData.config_path + (settingsData.exists === false ? " (not created)" : "")
    : "~/.config/agentpit/config.toml";
  insp.appendChild(foot);
  holder.appendChild(insp);
}

function field(label, control, opts) {
  const f = el("div", "ws-field" + (opts && opts.mono ? " mono" : ""));
  const l = el("label", opts && opts.ac ? "ac" : null, label);
  f.append(l, control);
  if (opts && opts.hint) { const p = el("p", "ws-fhint", opts.hint); f.appendChild(p); }
  return f;
}
function textInput(value, onInput, onChange, opts) {
  const i = el("input"); i.type = "text"; i.value = value == null ? "" : value;
  if (opts && opts.ac) i.classList.add("ac");
  if (opts && opts.placeholder) i.placeholder = opts.placeholder;
  if (opts && opts.disabled) i.disabled = true;
  if (onInput) i.addEventListener("input", () => onInput(i.value));
  if (onChange) i.addEventListener("change", () => onChange(i.value));
  return i;
}
function textArea(value, onInput, onChange, opts) {
  const t = el("textarea"); t.value = value == null ? "" : value;
  if (opts && opts.ac) t.classList.add("ac");
  if (onInput) t.addEventListener("input", () => onInput(t.value));
  if (onChange) t.addEventListener("change", () => onChange(t.value));
  return t;
}
function backendSelect(value, onChange, known, withEmpty) {
  const sel = el("select");
  if (withEmpty) { const o = el("option", null, "(default)"); o.value = ""; sel.appendChild(o); }
  for (const kb of known) { const o = el("option", null, kb); o.value = kb; sel.appendChild(o); }
  sel.value = value || "";
  sel.addEventListener("change", () => onChange(sel.value));
  return sel;
}

function inspStep(bd, st) {
  const form = el("div", "ws-form");
  const known = (settingsDraft && settingsDraft.known_backends) || Object.keys(CLI_NAME);
  form.appendChild(el("p", "ws-note", "This step is a blueprint (draft). Only the cast and workflow settings save; the manager improvises the decomposition at runtime."));
  form.appendChild(field("Step name / phase", textInput(st.name, (v) => setStepField(st.id, "name", v, false), (v) => setStepField(st.id, "name", v, true))));
  form.appendChild(field("manager backend (launching agent, illustrative)", backendSelect(st.manager, (v) => setStepField(st.id, "manager", v, true), known)));
  form.appendChild(field("PERSONA (viewpoint)", textArea(st.persona, (v) => setStepField(st.id, "persona", v, false), (v) => setStepField(st.id, "persona", v, true))));
  form.appendChild(field("BEHAVIOR / DIRECTIVE (the manager's instruction)", textArea(st.behavior, (v) => setStepField(st.id, "behavior", v, false), (v) => setStepField(st.id, "behavior", v, true), { ac: true }), { ac: true }));

  // workers
  const wf = el("div", "ws-field");
  wf.appendChild(el("label", null, "Workers (roles / CLIs this step runs)"));
  (st.workers || []).forEach((w, i) => {
    const rw = resolveWorker(w);
    const row = el("div", "ws-row");
    if (rw.mono) row.appendChild(mono(rw.mono, rw.color));
    else { const sw = el("span"); sw.style.cssText = `width:22px;height:22px;border-radius:5px;flex:none;background:${rw.color};`; row.appendChild(sw); }
    row.appendChild(el("span", "nm", rw.label));
    row.appendChild(el("span", "kind", rw.kind + (rw.known ? "" : "?")));
    const rm = el("button", "ws-mini", "✕"); rm.type = "button"; rm.addEventListener("click", () => removeWorker(st.id, i));
    row.appendChild(rm);
    wf.appendChild(row);
  });
  const addSel = el("select", "ws-add-sel");
  const o0 = el("option", null, "+ Add worker"); o0.value = ""; addSel.appendChild(o0);
  for (const r of castRoles()) if (r.name) { const o = el("option", null, "Role: " + r.name); o.value = "role:" + r.name; addSel.appendChild(o); }
  for (const c of cliInventory()) { const o = el("option", null, "CLI: " + c.label); o.value = "cli:" + c.id; addSel.appendChild(o); }
  addSel.addEventListener("change", () => { const v = addSel.value; addSel.value = ""; if (!v) return; const [k, id] = v.split(":"); addWorker(st.id, { type: k, id }); });
  wf.appendChild(addSel);
  form.appendChild(wf);

  // toggles
  const tg = el("div", "ws-toggles");
  tg.appendChild(checkRow("Allow dynamic spawn (sub-workflow generation)", st.dynamic, (v) => setStepField(st.id, "dynamic", v, true)));
  tg.appendChild(checkRow("Allow asking a human (ask_human)", st.ask, (v) => setStepField(st.id, "ask", v, true)));
  const rr = el("div", "ws-range-row");
  const rl = el("label"); rl.append(document.createTextNode("fan-out limit "), el("span", null, String(st.fanout || 1)));
  const range = el("input"); range.type = "range"; range.min = "1"; range.max = "8"; range.value = String(st.fanout || 1);
  range.addEventListener("input", () => { rl.querySelector("span").textContent = range.value; setStepField(st.id, "fanout", parseInt(range.value, 10), false); });
  rr.append(rl, range);
  tg.appendChild(rr);
  form.appendChild(tg);

  const del = el("button", "ws-del", "Delete this step"); del.type = "button"; del.addEventListener("click", () => deleteStep(st.id));
  form.appendChild(del);
  bd.appendChild(form);
  return { title: st.index + " · " + st.name, sub: "Workflow step (draft)" };
}
function checkRow(label, checked, onChange) {
  const l = el("label", "ws-check");
  const i = el("input"); i.type = "checkbox"; i.checked = !!checked;
  i.addEventListener("change", () => onChange(i.checked));
  l.append(i, document.createTextNode(label));
  return l;
}

function inspRole(bd, key) {
  const r = roleByKey(key);
  if (!r) return { title: "Nothing selected", sub: "" };
  const form = el("div", "ws-form");
  const known = (settingsDraft && settingsDraft.known_backends) || Object.keys(CLI_NAME);
  const isManager = r.name === "manager";

  const nameInput = textInput(r.name, (v) => setRoleField(key, "name", v.trim(), false), (v) => setRoleField(key, "name", v.trim(), true), { placeholder: "role-name", disabled: !r.isNew });
  const nameField = field("Role name", nameInput, { mono: true, hint: r.isNew ? "Lowercase letters, digits, -, _ (start alphanumeric)." : "A saved role name cannot be changed (delete and re-create)." });
  nameField.classList.add("mono");
  const err = el("p", "ws-fhint err hidden"); err.id = `ws-role-err-${key}`;
  nameField.appendChild(err);
  form.appendChild(nameField);
  if (isManager) form.appendChild(el("p", "ws-fhint", "Reserved role: the first claude / codex in the list becomes the orchestrator."));

  const bkField = el("div", "ws-field");
  bkField.appendChild(el("label", null, "BACKENDS (preference order — first wins)"));
  (r.backends || []).forEach((bid, i) => {
    const m = metaFor(bid);
    const row = el("div", "ws-row");
    row.appendChild(mono(m.mono, m.color));
    row.appendChild(el("span", "nm", m.label));
    const up = el("button", "ws-mini", "↑"); up.type = "button"; up.disabled = i === 0; up.addEventListener("click", () => moveBackend(key, i, i - 1));
    const dn = el("button", "ws-mini", "↓"); dn.type = "button"; dn.disabled = i === r.backends.length - 1; dn.addEventListener("click", () => moveBackend(key, i, i + 1));
    const rm = el("button", "ws-mini", "✕"); rm.type = "button"; rm.addEventListener("click", () => removeBackendAt(key, i));
    row.append(up, dn, rm);
    bkField.appendChild(row);
  });
  const addSel = el("select", "ws-add-sel");
  const o0 = el("option", null, "+ Add backend"); o0.value = ""; addSel.appendChild(o0);
  for (const kb of known) { const o = el("option", null, kb); o.value = kb; addSel.appendChild(o); }
  addSel.addEventListener("change", () => { const v = addSel.value; addSel.value = ""; if (v) addBackendTo(key, v); });
  bkField.appendChild(addSel);
  form.appendChild(bkField);

  form.appendChild(field("Prompt (persona)", textArea(r.prompt, (v) => setRoleField(key, "prompt", v, false), null)));
  form.appendChild(field("Model (optional)", textInput(r.model, (v) => setRoleField(key, "model", v, false), null, { placeholder: "e.g. opus / gpt-5-codex (empty = backend default)" }), { mono: true }));

  const del = el("button", "ws-del", "Delete this role"); del.type = "button"; del.addEventListener("click", () => deleteRole(key));
  form.appendChild(del);
  bd.appendChild(form);
  requestAnimationFrame(() => updateRoleNameErr(r));
  return { title: r.name || "(unnamed role)", sub: "role / cast" };
}

// A read-only key/value table (`.ws-kv`) from [key, value] pairs — the inspector's fact list.
function kvList(pairs) {
  const kv = el("div", "ws-kv");
  for (const [k, v] of pairs) {
    const row = el("div");
    row.append(el("span", "k", k), el("span", "v", v));
    kv.appendChild(row);
  }
  return kv;
}

function inspCli(bd, id) {
  const inv = cliInventory().find((c) => c.id === id) || { id, label: CLI_NAME[id] || id, version: "—", transport: CLI_TRANSPORT[id] || "exec", installed: false, path: "", command: id, note: "" };
  const m = metaFor(id);
  const form = el("div", "ws-form");
  const idRow = el("div", "ws-cli-id");
  idRow.appendChild(mono(m.mono, m.color));
  const col = el("div");
  col.append(el("div", "nm", inv.label), el("div", "note", inv.note || (inv.installed ? "installed" : "not on PATH")));
  idRow.appendChild(col);
  form.appendChild(idRow);
  const pairs = [
    ["command", inv.command],
    ["version", inv.version],
    ["transport", inv.transport],
    ["state", inv.installed ? "installed" : "missing"],
  ];
  if (inv.path) pairs.push(["path", inv.path]);
  form.appendChild(kvList(pairs));
  form.appendChild(el("p", "ws-fhint", "Assign it to a step's manager or workers. Manage versions from 'CLI versions' in the footer."));
  bd.appendChild(form);
  return { title: inv.label, sub: "agent CLI" };
}
function inspGen(bd, g) {
  const form = el("div", "ws-form");
  form.appendChild(el("p", "ws-note", "An illustration of a step spawned at runtime. Not editable in the blueprint (spawning is controlled by the parent step's dynamic spawn)."));
  form.appendChild(kvList([
    ["phase", g.name],
    ["backend", metaFor(g.backend).label],
    ["source", "review → refute"],
  ]));
  bd.appendChild(form);
  return { title: g.name, sub: "auto-generated step" };
}
// The workflow (root) inspector — edits the CURRENT workflow: the base [workflow] or a named type.
// The runtime GOAL is not an input here (agentpit interprets it at run time); what this edits is
// the reusable configuration + (for a type) which roles it casts and its manager brief.
function inspGoal(bd) {
  if (!settingsDraft) {
    bd.appendChild(el("p", "ws-fhint", "Loading workflow settings…"));
    return { title: "Workflow", sub: "" };
  }
  const t = currentTypeObj();
  return t ? inspWorkflowType(bd, t) : inspWorkflowBase(bd);
}
function inspWorkflowBase(bd) {
  const form = el("div", "ws-form");
  form.appendChild(el("p", "ws-note", "The default workflow ([workflow]). The runtime goal is interpreted by agentpit, so it is not entered here. Named workflows come from the selector above or ✨ Generate."));
  const invoke = el("div", "ws-field");
  invoke.appendChild(el("label", null, "INVOKE"));
  invoke.appendChild(el("div", "ws-code", 'agentpit workflow "<goal>"'));
  form.appendChild(invoke);
  const known = settingsDraft.known_backends || Object.keys(CLI_NAME);
  form.appendChild(field("manager backend (default orchestrator)", backendSelect(settingsDraft.workflow.manager_backend, (v) => setWorkflowField("manager_backend", v), known, true), { hint: "roles.manager takes precedence if set." }));
  form.appendChild(numRow("max depth (recursion ceiling)", settingsDraft.workflow.max_depth, (v) => setWorkflowField("max_depth", v)));
  form.appendChild(numRow("max calls / manager (per-manager dispatch budget)", settingsDraft.workflow.max_calls_per_manager, (v) => setWorkflowField("max_calls_per_manager", v)));
  const tg = el("div", "ws-toggles");
  tg.appendChild(checkRow("Run via MCP (use_mcp)", settingsDraft.workflow.use_mcp, (v) => setWorkflowField("use_mcp", v)));
  tg.appendChild(checkRow("Enable asking a human (enable_ask_human)", settingsDraft.workflow.enable_ask_human, (v) => setWorkflowField("enable_ask_human", v)));
  form.appendChild(tg);
  bd.appendChild(form);
  return { title: "(default) workflow", sub: "base [workflow]" };
}
function inspWorkflowType(bd, t) {
  const form = el("div", "ws-form");
  const known = settingsDraft.known_backends || Object.keys(CLI_NAME);

  const nameInput = textInput(
    t.name,
    (v) => setTypeField(t._key, "name", v.trim(), false),
    (v) => setTypeField(t._key, "name", v.trim(), true),
    { placeholder: "workflow-name", disabled: !t.isNew }
  );
  const nameField = field("Workflow name (type)", nameInput, {
    mono: true,
    hint: t.isNew
      ? "Lowercase letters, digits, -, _. Invoke with agentpit workflow <name> \"<goal>\"."
      : "A saved name cannot be changed (delete and re-create).",
  });
  const err = el("p", "ws-fhint err hidden");
  err.id = `ws-type-err-${t._key}`;
  nameField.appendChild(err);
  form.appendChild(nameField);

  const invoke = el("div", "ws-field");
  invoke.appendChild(el("label", null, "INVOKE"));
  invoke.appendChild(el("div", "ws-code", `agentpit workflow ${t.name || "<name>"} "<goal>"`));
  form.appendChild(invoke);

  form.appendChild(field("Display name (optional)", textInput(t.title, (v) => setTypeField(t._key, "title", v, false), null, { placeholder: "Strict code review" })));
  form.appendChild(field("BRIEF (the manager instruction for this workflow)", textArea(t.prompt, (v) => setTypeField(t._key, "prompt", v, false), null, { ac: true }), { ac: true }));

  const rolesField = el("div", "ws-field");
  rolesField.appendChild(el("label", null, "Roles used (none selected = all worker roles)"));
  const pool = workerRoleNames();
  if (!pool.length) {
    rolesField.appendChild(el("p", "ws-fhint", "Add roles (the cast) in the palette first."));
  } else {
    const box = el("div", "ws-toggles");
    for (const rn of pool) box.appendChild(checkRow(rn, t.roles.includes(rn), (on) => toggleTypeRole(t._key, rn, on)));
    rolesField.appendChild(box);
  }
  form.appendChild(rolesField);

  form.appendChild(el("p", "ws-fhint", "— overrides below (empty / inherit = fall back to base [workflow]) —"));
  form.appendChild(field("manager backend", backendSelect(t.manager_backend, (v) => setTypeField(t._key, "manager_backend", v, false), known, true)));
  form.appendChild(numRowNullable("max depth", t.max_depth, (v) => setTypeField(t._key, "max_depth", v, false)));
  form.appendChild(numRowNullable("max calls / manager", t.max_calls_per_manager, (v) => setTypeField(t._key, "max_calls_per_manager", v, false)));
  form.appendChild(triStateRow("Via MCP (use_mcp)", t.use_mcp, (v) => setTypeField(t._key, "use_mcp", v, false)));
  form.appendChild(triStateRow("Ask a human (enable_ask_human)", t.enable_ask_human, (v) => setTypeField(t._key, "enable_ask_human", v, false)));

  const del = el("button", "ws-del", "Delete this workflow");
  del.type = "button";
  del.addEventListener("click", () => deleteType(t._key));
  form.appendChild(del);
  bd.appendChild(form);
  requestAnimationFrame(() => updateTypeNameErr(t));
  return { title: t.title || t.name || "(unnamed workflow)", sub: "named workflow / type" };
}
function numRow(label, value, onChange) {
  const i = el("input"); i.type = "number"; i.min = "0"; i.step = "1"; i.value = value == null ? "" : String(value);
  i.addEventListener("input", () => { const n = parseInt(i.value, 10); onChange(Number.isFinite(n) ? n : 0); });
  return field(label, i);
}
// Nullable number field for per-type overrides: an empty input means null (inherit the base).
function numRowNullable(label, value, onChange) {
  const i = el("input"); i.type = "number"; i.min = "0"; i.step = "1";
  i.value = value == null ? "" : String(value);
  i.placeholder = "inherit";
  i.addEventListener("input", () => {
    const s = i.value.trim();
    if (s === "") return onChange(null);
    const n = parseInt(s, 10);
    onChange(Number.isFinite(n) ? n : null);
  });
  return field(label, i);
}
// Tri-state boolean for per-type overrides: inherit(null) / on(true) / off(false).
function triStateRow(label, value, onChange) {
  const sel = el("select");
  for (const [v, l] of [["", "inherit"], ["true", "on"], ["false", "off"]]) {
    const o = el("option", null, l); o.value = v; sel.appendChild(o);
  }
  sel.value = value == null ? "" : value ? "true" : "false";
  sel.addEventListener("change", () => onChange(sel.value === "" ? null : sel.value === "true"));
  return field(label, sel);
}

// ── visibility / lifecycle ───────────────────────────────────────────────────
function updateSettings() {
  const root = document.getElementById("settings");
  root.classList.toggle("hidden", !showSettings);
}
function refreshStudioClis() {
  if (showSettings && studioBuilt) renderStudio();
}
function toggleSettings() {
  showSettings = !showSettings;
  if (showSettings) {
    showSwarm = false;
    showCliManager = false;
    swarmSig = null;
    cliSig = null;
    updateSwarm();
    updateCliManager();
    buildStudioShell();
    if (!studio) newStudio();
    if (!settingsDraft) fetchSettings();
    else renderStudio();
    if (agentClis.length === 0) fetchAgentClis().then(refreshStudioClis);
    updateSettings();
    requestAnimationFrame(() => { if (showSettings) { fitToView(); renderStudio(); } });
  } else {
    updateSettings();
  }
}
async function fetchSettings() {
  settingsLoading = true;
  settingsError = null;
  if (studioBuilt) renderTopbar();
  try {
    const data = await invoke("settings_get");
    settingsData = data;
    settingsDraft = draftFromSettings(data);
    settingsDirty = false;
  } catch (e) {
    settingsError = String(e);
  } finally {
    settingsLoading = false;
    if (studioBuilt) renderStudio();
  }
}
async function saveSettings() {
  if (!settingsDraft || settingsSaving) return;
  for (const role of settingsDraft.roles) updateRoleNameErr(role);
  const { ok } = validateSettings(settingsDraft);
  if (!ok) { renderTopbar(); return; }
  settingsSaving = true;
  renderTopbar();
  try {
    const payload = {
      workflow: {
        manager_backend: settingsDraft.workflow.manager_backend || null,
        default_agents: settingsDraft.workflow.default_agents,
        max_depth: settingsDraft.workflow.max_depth,
        max_calls_per_manager: settingsDraft.workflow.max_calls_per_manager,
        use_mcp: settingsDraft.workflow.use_mcp,
        enable_ask_human: settingsDraft.workflow.enable_ask_human,
      },
      roles: settingsDraft.roles.map((r) => ({ name: r.name, backends: r.backends, prompt: r.prompt, model: r.model || null })),
      types: (settingsDraft.types || []).map((t) => ({
        name: t.name,
        title: t.title || null,
        prompt: t.prompt || null,
        roles: t.roles || [],
        manager_backend: t.manager_backend || null,
        max_depth: t.max_depth,
        max_calls_per_manager: t.max_calls_per_manager,
        use_mcp: t.use_mcp,
        enable_ask_human: t.enable_ask_human,
      })),
    };
    await invoke("settings_save", { payload });
    showToast("Settings saved.", "#4ec9a0");
    await fetchSettings();
  } catch (e) {
    settingsError = String(e);
    showToast(String(e), "var(--err)");
  } finally {
    settingsSaving = false;
    if (studioBuilt) renderStudio();
  }
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
  showToast(`Sent "${value}". Applying to the swarm.`, "#4ec9a0");
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
  if (showSettings) {
    if (e.key === "Escape") {
      toggleSettings();
      e.preventDefault();
    }
    return;
  }
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
  showToast(available ? "Welcome back." : "Going away. The swarm continues on the safe side.", "var(--ac)");
  renderAll();
}
function toggleSwarm() {
  showSwarm = !showSwarm;
  if (showSwarm) {
    showCliManager = false;
    showSettings = false;
    updateSettings();
  }
  swarmSig = null;
  updateSwarm();
}

function toggleCliManager() {
  showCliManager = !showCliManager;
  if (showCliManager) {
    showSwarm = false;
    showSettings = false;
    swarmSig = null;
    updateSwarm();
    updateSettings();
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
    refreshStudioClis(); // the Studio palette reads the same inventory
    // LATEST is best-effort and arrives after INSTALLED has already rendered, so its
    // arrival only re-signs the panel (fillCliDynamic) — the shell is never rebuilt.
    fetchLatestVersions();
  }
}

// Best-effort upstream check against the public registry. Never sets cliManagerError:
// a failed lookup simply leaves LATEST as '—' (unknown) with no banner.
async function fetchLatestVersions() {
  await Promise.allSettled(
    Object.entries(CLI_NPM).map(async ([id, pkg]) => {
      try {
        const res = await fetch(`https://registry.npmjs.org/${pkg}/latest`, {
          headers: { Accept: "application/json" },
        });
        if (!res.ok) return;
        const data = await res.json();
        if (data && data.version) cliLatest[id] = data.version;
      } catch (e) {
        /* offline / registry down → leave undefined so the row shows '—' (unknown) */
      }
    })
  );
  updateCliManager();
}

async function updateAgentCli(cli) {
  if (!cli.canUpdate || cliUpdating !== null) return;
  cliUpdating = cli.id;
  cliManagerError = null;
  updateCliManager();
  try {
    const result = await invoke("update_agent_cli", { id: cli.id });
    agentClis = agentClis.map((item) => (item.id === cli.id ? result.cli : item));
    showToast(`Updated ${cli.label} to ${result.cli.version || "the latest"}.`, "#4ec9a0");
  } catch (e) {
    const message = String(e);
    cliManagerError = message;
    showToast(`Failed to update ${cli.label}.`, "var(--err)");
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
      const title = fresh.length === 1 ? "One decision, please" : `${fresh.length} decisions waiting`;
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
  scheduleRender();
}
async function fetchSnapshot() {
  try {
    snapshot = (await invoke("get_snapshot")) || { live: [], recent: [] };
    connected = true;
  } catch (e) {
    connected = false;
  }
  scheduleRender();
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
  document.getElementById("settings-toggle").addEventListener("click", toggleSettings);
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
      scheduleRender();
    });
  } catch (e) {
    /* polling still covers it */
  }
}

boot();
