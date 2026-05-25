// agentpit dashboard frontend — vanilla JS, no build step.
// Polls get_snapshot every second (push events are a bonus) and re-renders. Clicking a
// member row opens an output panel that live-tails that backend's captured stdout.

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
  explain: "explain",
  refactor: "refactor",
  ensemble: "ensemble",
};

let lastSnapshot = { live: [], recent: [] };
// Member output panels currently open, keyed by `${runId}::${backend}::${agg}`.
const openOutputs = new Set();
// Recent runs expanded to show their member rows, keyed by runId.
const openRuns = new Set();
// Last fetched output text per member key, so DOM rebuilds don't flash "…".
const outputCache = new Map();

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

// A member row plus, when open, its output panel. Clicking the row toggles the panel.
function memberBlock(runId, m, nowMs) {
  const frag = document.createDocumentFragment();
  const key = memberKey(runId, m);
  const isOpen = openOutputs.has(key);

  const row = el("div", "member clickable" + (m.aggregator ? " agg" : "") + (isOpen ? " open" : ""));
  row.appendChild(el("span", "caret", isOpen ? "▾" : "▸"));
  row.appendChild(el("span", `ico s-${m.status}`, STATUS_ICON[m.status] || "·"));
  row.appendChild(el("span", "name", m.backend));

  const bar = el("div", "bar");
  if (m.status === "running") {
    bar.classList.add("indef");
    bar.appendChild(el("i"));
  }
  row.appendChild(bar);
  row.appendChild(el("span", "meta", memberMeta(m, nowMs)));
  row.addEventListener("click", () => {
    if (openOutputs.has(key)) openOutputs.delete(key);
    else openOutputs.add(key);
    render();
  });
  frag.appendChild(row);

  if (m.error && (m.status === "error" || m.status === "interrupted")) {
    frag.appendChild(el("div", "err-line", m.error.split("\n")[0].slice(0, 200)));
  }
  if (isOpen) {
    const pre = el("pre", "output");
    pre.id = "out::" + key;
    pre.dataset.key = key;
    pre.dataset.runId = runId;
    pre.dataset.backend = m.backend;
    pre.dataset.agg = m.aggregator ? "1" : "0";
    pre.textContent = outputCache.get(key) ?? "…";
    frag.appendChild(pre);
  }
  return frag;
}

function renderLive(runs, nowMs) {
  const box = document.getElementById("live");
  box.innerHTML = "";
  document.getElementById("live-count").textContent = runs.length;
  document.getElementById("live-empty").classList.toggle("hidden", runs.length > 0);

  for (const run of runs) {
    const card = el("div", "run");
    const head = el("div", "run-head");
    head.appendChild(el("span", "kind", KIND_LABEL[run.kind] || run.kind));
    head.appendChild(el("span", "cwd", run.cwd));
    head.appendChild(el("span", "run-elapsed", run.started_ts ? fmtMs(Math.max(0, nowMs - run.started_ts)) : ""));
    card.appendChild(head);
    for (const m of run.members) card.appendChild(memberBlock(run.run_id, m, nowMs));
    box.appendChild(card);
  }
}

function renderRecent(runs) {
  const box = document.getElementById("recent");
  box.innerHTML = "";
  document.getElementById("recent-empty").classList.toggle("hidden", runs.length > 0);

  for (const run of runs) {
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
    row.addEventListener("click", () => {
      if (openRuns.has(run.run_id)) openRuns.delete(run.run_id);
      else openRuns.add(run.run_id);
      render();
    });
    box.appendChild(row);

    if (expanded) {
      const sub = el("div", "recent-members");
      for (const m of run.members) sub.appendChild(memberBlock(run.run_id, m, Date.now()));
      box.appendChild(sub);
    }
  }
}

function render() {
  const now = Date.now();
  renderLive(lastSnapshot.live, now);
  renderRecent(lastSnapshot.recent);
  refreshOpenOutputs();
}

// Fill every visible output panel by fetching its captured log. Preserves scroll unless
// the user was already pinned to the bottom (so live output keeps following).
async function refreshOpenOutputs() {
  const panels = document.querySelectorAll("pre.output");
  for (const pre of panels) {
    try {
      const text = await invoke("get_output", {
        runId: pre.dataset.runId,
        backend: pre.dataset.backend,
        aggregator: pre.dataset.agg === "1",
      });
      const atBottom = pre.scrollHeight - pre.scrollTop - pre.clientHeight < 24;
      const display = text && text.length ? text : "(no output captured yet)";
      outputCache.set(pre.dataset.key, display);
      pre.textContent = display;
      if (atBottom) pre.scrollTop = pre.scrollHeight;
    } catch (e) {
      pre.textContent = "(failed to read output)";
    }
  }
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
    render();
    markUpdated();
  } catch (e) {
    setConnected(false);
    console.error("get_snapshot failed", e);
  }
}

async function boot() {
  await refresh();
  try {
    await listen("snapshot", (event) => {
      lastSnapshot = event.payload;
      setConnected(true);
      render();
      markUpdated();
    });
  } catch (e) {
    console.error("listen failed", e);
  }
  // Poll every second: fresh data + advance live timers + tail open output panels.
  setInterval(refresh, 1000);
  // Smooth the running clocks between fetches.
  setInterval(() => {
    if (lastSnapshot.live.length > 0) renderLive(lastSnapshot.live, Date.now());
  }, 500);
}

boot();
