import { useCallback, useEffect, useMemo, useState } from "react";
import * as api from "./bridge.js";
import {
  shortId,
  stageText,
  assignees,
  budgetUse,
  boardSections,
  inboxItems,
  answeredText,
  launcherCounts,
  ago,
} from "./format.js";
import { listSketches, blueprintNameFor, sketchToBlueprint } from "./import.js";
import { newBlueprint } from "./canvas.js";
import LoopCanvas from "./LoopCanvas.jsx";
import StartDialog from "./StartDialog.jsx";
import { makeT, detectLang } from "../studio/i18n.js";
import "./styles.css";

// Workspace loops (design §12): a board of every loop, an inbox of every decision waiting
// for a person (loop gates and the existing asks), and the blueprints to start them from.
// Same island shape as Workflow run / Learning: its own container, launcher and overlay.
// Everything comes from the bridge (dashboard/src-tauri/src/bridge), which folds loop
// journals with the CLI's code, so this view never re-derives loop state.

const ASK_POLL_MS = 2000;
const PROJECT_KEY = "agentpit.loops.project";

function loadProject() {
  try {
    return localStorage.getItem(PROJECT_KEY) || "";
  } catch {
    return "";
  }
}

function saveProject(p) {
  try {
    localStorage.setItem(PROJECT_KEY, p);
  } catch {
    // best-effort
  }
}

export default function LoopsApp() {
  const [open, setOpen] = useState(false);
  const [tab, setTab] = useState("board");
  const [rows, setRows] = useState([]);
  const [status, setStatus] = useState(null);
  const [inbox, setInbox] = useState({ gates: [], hidden: 0, answered: [] });
  const [asks, setAsks] = useState([]);
  const [canvas, setCanvas] = useState(null);
  const [error, setError] = useState(null);
  const [lang, setLang] = useState(detectLang);
  const t = useMemo(() => makeT(lang), [lang]);

  useEffect(() => {
    if (!api.available()) return undefined;
    const offs = [
      api.on("loops:board", (p) => {
        setRows(p.rows || []);
        setStatus(p.status || null);
      }),
      api.on("loops:inbox", (p) => setInbox(p || { gates: [], hidden: 0, answered: [] })),
      api.on("daemon:status", setStatus),
    ];
    api
      .board()
      .then((s) => {
        setRows(s.rows || []);
        setStatus(s.status || null);
        setInbox(s.inbox || { gates: [], hidden: 0, answered: [] });
      })
      .catch((e) => setError(e.message));
    return () => offs.forEach((off) => off());
  }, []);

  // The existing asks (get_pending_asks) have no push channel: poll, faster while open.
  useEffect(() => {
    if (!api.available()) return undefined;
    let alive = true;
    const tick = async () => {
      try {
        const a = await api.pendingAsks();
        if (alive) setAsks(a || []);
      } catch {
        // the legacy inbox reports connectivity
      }
      if (alive) setLang(detectLang());
    };
    tick();
    const id = setInterval(tick, open ? ASK_POLL_MS : ASK_POLL_MS * 5);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, [open]);

  useEffect(() => {
    if (!open) return undefined;
    const onKey = (e) => {
      if (e.key !== "Escape") return;
      if (canvas) setCanvas(null);
      else setOpen(false);
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [open, canvas]);

  const counts = useMemo(() => launcherCounts(rows, inbox, asks), [rows, inbox, asks]);
  const items = useMemo(() => inboxItems(inbox, asks), [inbox, asks]);

  const openLoop = useCallback((loopId) => {
    setCanvas({ mode: "run", loopId });
    setOpen(true);
  }, []);

  return (
    <>
      <button
        className={`lp-launcher ${counts.running ? "live" : ""} ${counts.decisions ? "ask" : ""}`}
        onClick={() => setOpen(true)}
      >
        <span className="lp-dot" />
        <span>{t("Loops")}</span>
        {counts.running ? <span className="lp-lc">{t("{n} running", { n: counts.running })}</span> : null}
        {counts.decisions ? <span className="lp-lc lp-lc-ask">{t("{n} waiting for you", { n: counts.decisions })}</span> : null}
      </button>
      {open ? (
        <div className="lp-overlay">
          <div className="lp-head">
            <span className="lp-title">{t("Loops")}</span>
            <nav className="lp-tabs">
              {[
                ["board", t("Board")],
                ["inbox", `${t("Inbox")}${items.length ? ` · ${items.length}` : ""}`],
                ["blueprints", t("Blueprints")],
              ].map(([id, label]) => (
                <button key={id} className={tab === id ? "on" : ""} onClick={() => setTab(id)}>
                  {label}
                </button>
              ))}
            </nav>
            <DaemonPill t={t} status={status} />
            <button className="lp-close" onClick={() => setOpen(false)}>
              {t("Close ✕")}
            </button>
          </div>
          {error ? <div className="lp-banner">{error}</div> : null}
          {!api.available() ? <div className="lp-banner">{t("The desktop bridge is not available.")}</div> : null}
          <div className="lp-body">
            {tab === "board" ? <Board t={t} rows={rows} onOpen={openLoop} /> : null}
            {tab === "inbox" ? <Inbox t={t} items={items} answered={inbox.answered || []} onOpen={openLoop} /> : null}
            {tab === "blueprints" ? (
              <Blueprints
                t={t}
                onDesign={(ref) => setCanvas({ mode: "design", ref })}
                onStarted={openLoop}
              />
            ) : null}
          </div>
          {canvas ? (
            <LoopCanvas
              t={t}
              initial={canvas}
              rows={rows}
              onClose={() => setCanvas(null)}
              onOpenLoop={openLoop}
            />
          ) : null}
        </div>
      ) : null}
    </>
  );
}

function DaemonPill({ t, status }) {
  if (!status) return null;
  const label =
    status.state === "connected"
      ? t("daemon connected")
      : status.state === "down"
        ? t("daemon unreachable")
        : t("connecting…");
  return (
    <span className={`lp-daemon lp-daemon-${status.state}`} title={status.message || ""}>
      <span className="lp-dot" />
      {label}
    </span>
  );
}

// ── Board ──────────────────────────────────────────────────────────────────────────────

const STATE_LABEL = {
  running: "running",
  waiting: "waiting for you",
  parked: "parked · waiting for you",
  stalled: "no runner",
  paused: "paused",
  stopping: "stopping",
  created: "not started",
  succeeded: "succeeded",
  failed: "failed",
  cancelled: "cancelled",
};

function Board({ t, rows, onOpen }) {
  const { active, finished } = useMemo(() => boardSections(rows), [rows]);
  const [showDone, setShowDone] = useState(false);
  if (!rows.length) {
    return <div className="lp-empty">{t("No loops yet — start one from Blueprints, or `agentpit loop start`.")}</div>;
  }
  return (
    <div className="lp-board">
      {active.map(({ row, state }) => (
        <BoardRow key={row.summary.loop_id} t={t} row={row} state={state} onOpen={onOpen} />
      ))}
      {finished.length ? (
        <button className="lp-more" onClick={() => setShowDone((v) => !v)}>
          {showDone ? t("Hide finished") : t("Show {n} finished", { n: finished.length })}
        </button>
      ) : null}
      {showDone
        ? finished.map(({ row, state }) => (
            <BoardRow key={row.summary.loop_id} t={t} row={row} state={state} onOpen={onOpen} />
          ))
        : null}
    </div>
  );
}

function BoardRow({ t, row, state, onOpen }) {
  const s = row.summary;
  const budget = budgetUse(s);
  const [busy, setBusy] = useState(null);
  const [err, setErr] = useState(null);
  const act = async (e, op) => {
    e.stopPropagation();
    if (op.op === "stop" && !window.confirm(t("Stop this loop? Running steps finish first."))) return;
    setBusy(op.op);
    setErr(null);
    try {
      await api.control(s.loop_id, op, api.newOpId());
    } catch (x) {
      setErr(x.message);
    } finally {
      setBusy(null);
    }
  };
  const live = !["succeeded", "failed", "cancelled"].includes(state);
  return (
    <div className={`lp-row lp-st-${state}`} onClick={() => onOpen(s.loop_id)}>
      <span className={`lp-state lp-state-${state}`}>{t(STATE_LABEL[state] || state)}</span>
      <div className="lp-row-main">
        <div className="lp-row-top">
          <span className="lp-row-title">{s.title || s.blueprint.name}</span>
          <span className="lp-row-id">{shortId(s.loop_id)}</span>
          <span className="lp-row-bp">{s.blueprint.name}</span>
        </div>
        <div className="lp-row-stage">{stageText(s) || "—"}</div>
        <div className="lp-row-cast">
          {assignees(s).map((a) => (
            <span key={`${a.node}-${a.who}`} className="lp-chip">
              {a.node}: {a.who}
            </span>
          ))}
          {s.open_gate_count ? <span className="lp-chip lp-chip-ask">{t("{n} gate(s) open", { n: s.open_gate_count })}</span> : null}
          {s.pending_instructions ? (
            <span className="lp-chip">{t("{n} instruction(s) queued", { n: s.pending_instructions })}</span>
          ) : null}
        </div>
        {err ? <div className="lp-err">{err}</div> : null}
      </div>
      <div className="lp-row-side">
        <div className="lp-bar" title={t("steps {v}", { v: budget.stepsText })}>
          <span style={{ width: `${budget.steps * 100}%` }} />
        </div>
        <div className="lp-bar" title={t("compute {v}", { v: budget.timeText })}>
          <span style={{ width: `${budget.time * 100}%` }} />
        </div>
        <span className="lp-ago">{ago(s.updated_ts)}</span>
        {live ? (
          <span className="lp-actions">
            {state === "paused" ? (
              <button disabled={!!busy} onClick={(e) => act(e, { op: "resume" })} title={t("Resume")}>
                <Icon name="play" />
              </button>
            ) : state === "created" ? (
              <button disabled={!!busy} onClick={(e) => act(e, { op: "start" })} title={t("Start")}>
                <Icon name="play" />
              </button>
            ) : (
              <button disabled={!!busy} onClick={(e) => act(e, { op: "pause", mode: "drain" })} title={t("Pause")}>
                <Icon name="pause" />
              </button>
            )}
            <button disabled={!!busy} onClick={(e) => act(e, { op: "stop", mode: "graceful" })} title={t("Stop")}>
              <Icon name="stop" />
            </button>
          </span>
        ) : null}
      </div>
    </div>
  );
}

// Small inline icons (font glyphs for these vary too much between webviews).
function Icon({ name }) {
  const paths = {
    play: "M3 2 L10 6 L3 10 Z",
    pause: "M3 2 H5 V10 H3 Z M7 2 H9 V10 H7 Z",
    stop: "M3 3 H9 V9 H3 Z",
  };
  return (
    <svg width="12" height="12" viewBox="0 0 12 12" aria-hidden="true">
      <path d={paths[name]} fill="currentColor" />
    </svg>
  );
}

// ── Inbox ──────────────────────────────────────────────────────────────────────────────

function Inbox({ t, items, answered, onOpen }) {
  return (
    <div className="lp-inbox">
      {items.length === 0 ? <div className="lp-empty">{t("Nothing is waiting for you.")}</div> : null}
      {items.map((it) => (
        <InboxCard key={it.key} t={t} item={it} onOpen={onOpen} />
      ))}
      {answered.length ? (
        <div className="lp-answered">
          <div className="lp-sub">{t("Recently answered")}</div>
          {answered.map((a) => (
            <div key={`${a.loop_id}:${a.gate_id}`} className="lp-answered-row" onClick={() => onOpen(a.loop_id)}>
              <span className="lp-answered-title">{a.loop_title}</span>
              <span className="lp-answered-prompt">{a.prompt}</span>
              <span className="lp-answered-how">{answeredText(a)}</span>
            </div>
          ))}
        </div>
      ) : null}
    </div>
  );
}

function InboxCard({ t, item, onOpen }) {
  const [comment, setComment] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState(null);
  const [opId] = useState(api.newOpId);
  const answer = async (option) => {
    setBusy(true);
    setErr(null);
    try {
      if (item.type === "gate") await api.resolveGate(item.loopId, item.gateId, option, comment || null, opId);
      else await api.answerAsk(item.askId, option);
    } catch (e) {
      setErr(e.message);
      setBusy(false);
    }
  };
  return (
    <div className={`lp-card lp-card-${item.type} ${item.overdue ? "overdue" : ""}`}>
      <div className="lp-card-hd">
        {item.type === "gate" ? (
          <>
            <button className="lp-link" onClick={() => onOpen(item.loopId)}>
              {item.title || item.blueprint}
            </button>
            <span className="lp-card-kind">{item.kind}</span>
            {item.node ? <span className="lp-card-node">{item.node}</span> : null}
            {item.parked ? <span className="lp-chip">{t("parked — answering wakes it")}</span> : null}
          </>
        ) : (
          <>
            <span className="lp-card-title">{t(item.title)}</span>
            <span className="lp-card-kind">ask</span>
          </>
        )}
        {item.deadline ? (
          <span className="lp-card-deadline">{new Date(item.deadline).toLocaleTimeString()}</span>
        ) : null}
      </div>
      <div className="lp-card-prompt">{item.prompt}</div>
      {item.type === "gate" ? (
        <input
          className="lp-input"
          placeholder={t("Comment (optional, passed to the next step)")}
          value={comment}
          onChange={(e) => setComment(e.target.value)}
        />
      ) : null}
      <div className="lp-card-opts">
        {item.options.map((o) => (
          <button
            key={o.id}
            disabled={busy}
            className={`lp-opt ${o.outcome === "fail" ? "neg" : o.outcome === "ok" ? "pos" : ""}`}
            onClick={() => answer(o.id)}
          >
            {t(o.label)}
          </button>
        ))}
      </div>
      {err ? <div className="lp-err">{err}</div> : null}
    </div>
  );
}

// ── Blueprints ─────────────────────────────────────────────────────────────────────────

function Blueprints({ t, onDesign, onStarted }) {
  const [project, setProject] = useState(loadProject);
  const [list, setList] = useState([]);
  const [err, setErr] = useState(null);
  const [starting, setStarting] = useState(null);
  const [importing, setImporting] = useState(false);

  const reload = useCallback(async () => {
    setErr(null);
    try {
      setList(await api.blueprints(project.trim() || null));
    } catch (e) {
      setErr(e.message);
    }
  }, [project]);

  useEffect(() => {
    reload();
  }, [reload]);

  const scopeFor = () => (project.trim() ? "project" : "user");

  const create = async () => {
    const name = window.prompt(t("Blueprint name (a-z, 0-9, - and _)"));
    if (!name) return;
    try {
      await api.saveBlueprint(scopeFor(), name, newBlueprint(name), null, project.trim() || null);
      onDesign({ scope: scopeFor(), name, project: project.trim() || null });
    } catch (e) {
      setErr(e.message);
    }
  };

  const remove = async (b) => {
    if (!window.confirm(t("Delete {name}?", { name: b.name }))) return;
    try {
      await api.deleteBlueprint(b.scope, b.name, b.rev, project.trim() || null);
      reload();
    } catch (e) {
      setErr(e.message);
    }
  };

  return (
    <div className="lp-bps">
      <div className="lp-bps-bar">
        <label className="lp-field">
          <span>{t("Project directory")}</span>
          <input
            className="lp-input"
            placeholder="/path/to/repo"
            value={project}
            onChange={(e) => setProject(e.target.value)}
            onBlur={() => saveProject(project.trim())}
          />
        </label>
        <button className="lp-btn" onClick={create}>
          {t("＋ New blueprint")}
        </button>
        <button className="lp-btn" onClick={() => setImporting((v) => !v)}>
          {t("Import a Studio sketch")}
        </button>
      </div>
      {err ? <div className="lp-err">{err}</div> : null}
      {importing ? (
        <ImportSketches
          t={t}
          scope={scopeFor()}
          project={project.trim() || null}
          onDone={(ref) => {
            setImporting(false);
            reload();
            if (ref) onDesign(ref);
          }}
        />
      ) : null}
      {list.length === 0 ? <div className="lp-empty">{t("No blueprints yet.")}</div> : null}
      {list.map((b) => (
        <div key={`${b.scope}:${b.name}`} className={`lp-bp ${b.shadowed ? "shadowed" : ""}`}>
          <span className={`lp-scope lp-scope-${b.scope}`}>{t(b.scope)}</span>
          <div className="lp-bp-main">
            <div className="lp-bp-name">
              {b.name}
              {b.title && b.title !== b.name ? <span className="lp-bp-title">{b.title}</span> : null}
            </div>
            {b.description ? <div className="lp-bp-desc">{b.description}</div> : null}
            <div className="lp-bp-diag">
              {b.errors ? <span className="lp-diag-err">{t("{n} error(s)", { n: b.errors })}</span> : null}
              {b.warnings ? <span className="lp-diag-warn">{t("{n} warning(s)", { n: b.warnings })}</span> : null}
              {b.shadowed ? <span>{t("hidden by the project blueprint of the same name")}</span> : null}
            </div>
          </div>
          <button className="lp-btn" onClick={() => onDesign({ scope: b.scope, name: b.name, project: project.trim() || null })}>
            {t("Design")}
          </button>
          <button className="lp-btn pri" disabled={!b.runnable} onClick={() => setStarting(b)}>
            {t("Start…")}
          </button>
          <button className="lp-btn ghost" onClick={() => remove(b)} title={t("Delete")}>
            ✕
          </button>
        </div>
      ))}
      {starting ? (
        <StartDialog
          t={t}
          entry={starting}
          project={project.trim() || null}
          onClose={() => setStarting(null)}
          onStarted={(loopId) => {
            setStarting(null);
            onStarted(loopId);
          }}
        />
      ) : null}
    </div>
  );
}

function ImportSketches({ t, scope, project, onDone }) {
  const sketches = useMemo(() => {
    try {
      return listSketches(window.localStorage);
    } catch {
      return [];
    }
  }, []);
  const [err, setErr] = useState(null);
  const [warnings, setWarnings] = useState([]);
  const importOne = async (s) => {
    const name = window.prompt(t("Save as blueprint"), blueprintNameFor(s.workflow));
    if (!name) return;
    const { doc, warnings: w } = sketchToBlueprint(s.sketch, name);
    try {
      await api.saveBlueprint(scope, name, doc, null, project);
      setWarnings(w);
      if (!w.length) onDone({ scope, name, project });
    } catch (e) {
      setErr(e.message);
    }
  };
  return (
    <div className="lp-import">
      <div className="lp-sub">{t("Studio sketches on this machine")}</div>
      {sketches.length === 0 ? <div className="lp-empty">{t("No drawn sketches found.")}</div> : null}
      {sketches.map((s) => (
        <div key={s.key} className="lp-import-row">
          <span>{s.workflow}</span>
          <span className="lp-muted">{t("{n} steps", { n: s.steps })}</span>
          <button className="lp-btn" onClick={() => importOne(s)}>
            {t("Import")}
          </button>
        </div>
      ))}
      {warnings.length ? (
        <div className="lp-warns">
          <div className="lp-sub">{t("Imported, with notes:")}</div>
          {warnings.map((w) => (
            <div key={w}>• {w}</div>
          ))}
          <button className="lp-btn" onClick={() => onDone(null)}>
            {t("OK")}
          </button>
        </div>
      ) : null}
      {err ? <div className="lp-err">{err}</div> : null}
    </div>
  );
}
