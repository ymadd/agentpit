// agentpit dashboard frontend — vanilla JS, no build step.
//
// Rendering is incremental so reading logs isn't disrupted: the DOM structure is rebuilt
// only when the *shape* changes (a run starts/ends, a panel is opened, a row expanded).
// In steady state we just patch text (elapsed, status) and append streamed output to the
// open <pre> panels — so scroll position and text selection survive across ticks.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

const STATUS_ICON = {
  running: "▶",
  ok: "✓",
  error: "✗",
  interrupted: "✗",
  skipped: "⊘",
  pending: "·",
};

const KIND_LABEL = {
  rescue: "rescue",
  review: "review",
  security_review: "security",
  adversarial_review: "adversarial",
  explain: "explain",
  refactor: "refactor",
  ensemble: "ensemble",
};

let lastSnapshot = { live: [], recent: [] };
const openOutputs = new Set(); // member keys with an output panel open
const openRuns = new Set(); // recent run_ids expanded to show members

// Rebuilt on each structural change; used by patch/stream passes in between.
let memberEls = new Map(); // key -> { row, icon, meta, bar, pre|null }
let runEls = new Map(); // run_id -> { elapsedEl|null }
let lastSig = null;

function memberKey(runId, m) {
  return `${runId}::${m.backend}::${m.aggregator ? 1 : 0}`;
}

function fmtMs(ms) {
  if (ms == null) return "";
  const s = ms / 1000;
  return s < 60 ? `${s.toFixed(1)}s` : `${Math.floor(s / 60)}m${Math.round(s % 60)}s`;
}
function fmtChars(n) {
  if (n == null) return "";
  return n < 1024 ? `${n} chars` : `${(n / 1024).toFixed(1)} KB`;
}
function fmtClock(ms) {
  if (!ms) return "";
  return new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

function el(tag, cls, text) {
  const e = document.createElement(tag);
  if (cls) e.className = cls;
  if (text != null) e.textContent = text;
  return e;
}

function memberMeta(m, nowMs) {
  if (m.status === "running" && m.started_ts) return fmtMs(Math.max(0, nowMs - m.started_ts));
  if (m.status === "ok") return [fmtMs(m.elapsed_ms), fmtChars(m.chars)].filter(Boolean).join("  ");
  if (m.status === "skipped") return "skipped";
  if (m.status === "interrupted") return "interrupted";
  if (m.status === "error") return `failed ${fmtMs(m.elapsed_ms)}`;
  return "queued";
}

// --- structural signature ------------------------------------------------
// Only things that require a DOM rebuild go in here: which runs exist, in which section,
// their member keys, which panels are open, which recent runs are expanded. Status,
// elapsed and output are deliberately excluded — those are patched in place.
function computeSig() {
  const part = (r, section) => {
    const members = r.members
      .map((m) => {
        const k = memberKey(r.run_id, m);
        return k + (openOutputs.has(k) ? "*" : "");
      })
      .join(",");
    return `${section}:${r.run_id}:${openRuns.has(r.run_id) ? "x" : ""}:${members}`;
  };
  return (
    lastSnapshot.live.map((r) => part(r, "L")).join("|") +
    "#" +
    lastSnapshot.recent.map((r) => part(r, "R")).join("|")
  );
}

// --- structure rebuild (rare) -------------------------------------------
function makeMemberBlock(runId, m, parent) {
  const key = memberKey(runId, m);
  const isOpen = openOutputs.has(key);

  const row = el("div", "member clickable" + (m.aggregator ? " agg" : "") + (isOpen ? " open" : ""));
  const caret = el("span", "caret", isOpen ? "▾" : "▸");
  const icon = el("span", `ico s-${m.status}`, STATUS_ICON[m.status] || "·");
  const name = el("span", "name", m.backend);
  const bar = el("div", "bar");
  if (m.status === "running") {
    bar.classList.add("indef");
    bar.appendChild(el("i"));
  }
  const meta = el("span", "meta", memberMeta(m, Date.now()));
  row.append(caret, icon, name, bar, meta);
  row.addEventListener("click", () => toggleOutput(key));
  parent.appendChild(row);

  let pre = null;
  if (isOpen) {
    pre = el("pre", "output");
    pre.dataset.runId = runId;
    pre.dataset.backend = m.backend;
    pre.dataset.agg = m.aggregator ? "1" : "0";
    pre.dataset.offset = "0";
    parent.appendChild(pre);
  }
  memberEls.set(key, { row, icon, meta, bar, pre });
}

function buildLiveCard(run) {
  const card = el("div", "run");
  const head = el("div", "run-head");
  head.appendChild(el("span", "kind", KIND_LABEL[run.kind] || run.kind));
  head.appendChild(el("span", "cwd", run.cwd));
  const elapsedEl = el("span", "run-elapsed", run.started_ts ? fmtMs(Date.now() - run.started_ts) : "");
  head.appendChild(elapsedEl);
  card.appendChild(head);
  for (const m of run.members) makeMemberBlock(run.run_id, m, card);
  runEls.set(run.run_id, { elapsedEl });
  return card;
}

function buildRecentRow(run) {
  const wrap = document.createDocumentFragment();
  const expanded = openRuns.has(run.run_id);
  const row = el("div", "recent-row clickable" + (expanded ? " open" : ""));
  row.appendChild(el("span", "caret", expanded ? "▾" : "▸"));
  row.appendChild(el("span", "kind", KIND_LABEL[run.kind] || run.kind));

  const ok = run.members.filter((m) => m.status === "ok").length;
  const bad = run.members.filter((m) => m.status === "error" || m.status === "interrupted").length;
  const skip = run.members.filter((m) => m.status === "skipped").length;
  const tally = el("span", "tally");
  if (ok) tally.appendChild(el("span", "s-ok", `✓${ok}`));
  if (bad) tally.appendChild(el("span", "s-error", `✗${bad}`));
  if (skip) tally.appendChild(el("span", "s-skipped", `⊘${skip}`));
  row.appendChild(tally);
  row.appendChild(el("span", "cwd", run.cwd));
  row.appendChild(el("span", "when", fmtClock(run.started_ts)));
  row.addEventListener("click", () => toggleRun(run.run_id));
  wrap.appendChild(row);

  if (expanded) {
    const sub = el("div", "recent-members");
    for (const m of run.members) makeMemberBlock(run.run_id, m, sub);
    wrap.appendChild(sub);
  }
  return wrap;
}

function rebuildStructure() {
  memberEls = new Map();
  runEls = new Map();

  const liveBox = document.getElementById("live");
  liveBox.innerHTML = "";
  document.getElementById("live-count").textContent = lastSnapshot.live.length;
  document.getElementById("live-empty").classList.toggle("hidden", lastSnapshot.live.length > 0);
  for (const run of lastSnapshot.live) liveBox.appendChild(buildLiveCard(run));

  const recentBox = document.getElementById("recent");
  recentBox.innerHTML = "";
  document.getElementById("recent-empty").classList.toggle("hidden", lastSnapshot.recent.length > 0);
  for (const run of lastSnapshot.recent) recentBox.appendChild(buildRecentRow(run));
}

// --- in-place patches (every tick, cheap) -------------------------------
function patchMember(runId, m, now) {
  const me = memberEls.get(memberKey(runId, m));
  if (!me) return;
  me.icon.textContent = STATUS_ICON[m.status] || "·";
  me.icon.className = `ico s-${m.status}`;
  me.meta.textContent = memberMeta(m, now);
  const running = m.status === "running";
  me.bar.classList.toggle("indef", running);
  if (running && !me.bar.firstChild) me.bar.appendChild(el("i"));
  if (!running && me.bar.firstChild) me.bar.innerHTML = "";
}

function patchDynamic() {
  const now = Date.now();
  for (const run of lastSnapshot.live) {
    const re = runEls.get(run.run_id);
    if (re && re.elapsedEl) re.elapsedEl.textContent = run.started_ts ? fmtMs(now - run.started_ts) : "";
    for (const m of run.members) patchMember(run.run_id, m, now);
  }
  for (const run of lastSnapshot.recent) {
    if (openRuns.has(run.run_id)) for (const m of run.members) patchMember(run.run_id, m, now);
  }
}

// --- output streaming (append-only) -------------------------------------
async function streamOutputs() {
  for (const { pre } of memberEls.values()) {
    if (!pre) continue;
    const offset = Number(pre.dataset.offset || "0");
    try {
      const chunk = await invoke("get_output", {
        runId: pre.dataset.runId,
        backend: pre.dataset.backend,
        aggregator: pre.dataset.agg === "1",
        offset,
      });
      if (chunk.reset) pre.textContent = "";
      if (chunk.text) {
        // Append only — never replace — so scroll and text selection survive.
        const atBottom = pre.scrollHeight - pre.scrollTop - pre.clientHeight < 28;
        pre.appendChild(document.createTextNode(chunk.text));
        if (atBottom) pre.scrollTop = pre.scrollHeight;
      }
      pre.dataset.offset = String(chunk.offset);
    } catch (e) {
      /* transient; try again next tick */
    }
  }
}

// --- reconcile ----------------------------------------------------------
function reconcile() {
  const sig = computeSig();
  if (sig !== lastSig) {
    rebuildStructure();
    lastSig = sig;
  }
  patchDynamic();
  streamOutputs();
}

function toggleOutput(key) {
  if (openOutputs.has(key)) openOutputs.delete(key);
  else openOutputs.add(key);
  reconcile();
}
function toggleRun(runId) {
  if (openRuns.has(runId)) openRuns.delete(runId);
  else openRuns.add(runId);
  reconcile();
}

function setConnected(ok) {
  document.getElementById("conn-dot").classList.toggle("connected", ok);
}
function markUpdated() {
  document.getElementById("updated").textContent = "updated " + new Date().toLocaleTimeString();
}

async function refresh() {
  try {
    lastSnapshot = await invoke("get_snapshot");
    setConnected(true);
    reconcile();
    markUpdated();
  } catch (e) {
    setConnected(false);
  }
}

async function boot() {
  await refresh();
  try {
    await listen("snapshot", (event) => {
      lastSnapshot = event.payload;
      setConnected(true);
      reconcile();
      markUpdated();
    });
  } catch (e) {
    console.error("listen failed", e);
  }
  // Snapshot refresh (also re-checks pid liveness) — modest cadence; it patches, not rebuilds.
  setInterval(refresh, 1500);
  // Tick elapsed/status text without touching structure.
  setInterval(patchDynamic, 1000);
  // Stream output deltas into open panels — frequent but cheap (append-only).
  setInterval(streamOutputs, 500);
}

boot();
