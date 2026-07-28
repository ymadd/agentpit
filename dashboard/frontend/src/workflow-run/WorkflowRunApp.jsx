import { useEffect, useMemo, useRef, useState } from "react";
import {
  ReactFlow,
  Background,
  Controls,
  MiniMap,
  useNodesState,
  useEdgesState,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import StageNode, { elapsed } from "./StageNode.jsx";
import { getSnapshot, listWorkflows, buildStageGraph, summarize, runStatus } from "./snapshot.js";
import { makeT, detectLang } from "../studio/i18n.js";
import "../reactflow-dark.css";
import "./styles.css";

const nodeTypes = { stage: StageNode };
const POLL_MS = 1500;

export default function WorkflowRunApp() {
  const [snapshot, setSnapshot] = useState({ live: [], recent: [] });
  const [open, setOpen] = useState(false);
  // The run the user pinned in the picker. null = follow the newest live run,
  // so an unattended dashboard keeps auto-advancing to whatever is running now.
  const [pinnedId, setPinnedId] = useState(null);
  // Re-read on every tick rather than wiring a change event: the language can be flipped
  // from the Studio toggle or the legacy app.js, neither of which notifies this island.
  const [lang, setLang] = useState(detectLang);
  const t = useMemo(() => makeT(lang), [lang]);

  useEffect(() => {
    let alive = true;
    const tick = async () => {
      const s = await getSnapshot();
      if (!alive) return;
      setSnapshot(s || { live: [], recent: [] });
      setLang(detectLang());
    };
    tick();
    const id = setInterval(tick, POLL_MS);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, []);

  // `listWorkflows` already sorts live-first, so its head IS what pickWorkflow would return.
  const workflows = useMemo(() => listWorkflows(snapshot), [snapshot]);
  const auto = workflows[0] || null;
  const root = (pinnedId && workflows.find((w) => w.run_id === pinnedId)) || auto;
  const liveCount = useMemo(
    () => (snapshot.live || []).filter((r) => r.kind === "workflow").length,
    [snapshot]
  );
  const info = useMemo(() => (root ? summarize(snapshot, root) : null), [snapshot, root]);
  // The launcher is an ambient "is anything running" indicator, so it always reports the
  // auto-picked (live) run — pinning an old run in the overlay must not blank it out.
  // Unpinned, that is the same run the overlay shows, so reuse the summary already computed.
  const autoInfo = useMemo(
    () => (auto && auto !== root ? summarize(snapshot, auto) : null),
    [snapshot, auto, root]
  );
  const launcherInfo = auto === root ? info : autoInfo;

  return (
    <>
      <button className={`wr-launcher ${liveCount ? "live" : ""}`} onClick={() => setOpen(true)}>
        <span className="wr-dot" />
        <span>{t("Workflow run")}</span>
        {launcherInfo ? (
          <span className="wr-count">
            {launcherInfo.running ? `${launcherInfo.running}▸ ` : ""}
            {t("{n} stages", { n: launcherInfo.total })}
          </span>
        ) : null}
      </button>
      {open && (
        <Overlay
          t={t}
          snapshot={snapshot}
          workflows={workflows}
          root={root}
          info={info}
          pinnedId={pinnedId}
          onPin={setPinnedId}
          onClose={() => setOpen(false)}
        />
      )}
    </>
  );
}

// Runs are identified by their working directory's last segment — the repo name, not the path.
function dirName(cwd) {
  return cwd ? cwd.split("/").pop() : "";
}

// A workflow's option label in the picker: when it started, its manager, and its state.
function runLabel(t, run, isLive) {
  const started = new Date(run.started_ts).toLocaleTimeString();
  const state = isLive ? t("live") : t(runStatus(run) === "failed" ? "failed" : "finished");
  return `${started} · ${dirName(run.cwd)} · ${state}`;
}

function Overlay({ t, snapshot, workflows, root, info, pinnedId, onPin, onClose }) {
  const [nodes, setNodes, onNodesChange] = useNodesState([]);
  const [edges, setEdges, onEdgesChange] = useEdgesState([]);
  const [selId, setSelId] = useState(null);
  // Ids the user dragged. Only these keep their position across a poll — everything else
  // re-flows, so stages dispatched later slot into the tidy tree instead of being frozen
  // into the layout that was correct when the run had two stages.
  const pinnedPos = useRef(new Map());

  useEffect(() => {
    const onKey = (e) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  // Rebuild on each poll, re-laying out the tree and restoring only dragged nodes.
  // `selected` is stamped from `selId` here rather than left to React Flow's own selection
  // state: this rebuild replaces the node objects every tick, so any selection React Flow
  // tracked internally would be dropped a second later. One source of truth, both for the
  // highlight ring and the detail panel.
  useEffect(() => {
    if (!root) {
      setNodes([]);
      setEdges([]);
      return;
    }
    const g = buildStageGraph(snapshot, root);
    const pinned = pinnedPos.current;
    setNodes(
      g.nodes.map((n) => ({
        ...n,
        selected: n.id === selId,
        position: pinned.get(n.id) || n.position,
      }))
    );
    setEdges(g.edges);
  }, [snapshot, root, selId, setNodes, setEdges]);

  // Switching runs drops the previous run's manual placements and detail panel.
  useEffect(() => {
    pinnedPos.current = new Map();
    setSelId(null);
  }, [root]);

  const selected = nodes.find((n) => n.id === selId) || null;
  const liveIds = new Set((snapshot.live || []).map((r) => r.run_id));

  return (
    <div className="wr-overlay">
      <div className="wr-head">
        <span className="wr-title">{t("Workflow run")}</span>
        {workflows.length > 1 ? (
          <select className="wr-pick" value={pinnedId || ""} onChange={(e) => onPin(e.target.value || null)}>
            {/* Empty value = un-pin and go back to following whatever is live. */}
            <option value="">{t("Follow the latest run")}</option>
            {workflows.map((w) => (
              <option key={w.run_id} value={w.run_id}>
                {runLabel(t, w, liveIds.has(w.run_id))}
              </option>
            ))}
          </select>
        ) : null}
        {info ? (
          <span className="wr-meta">
            {info.manager} · {dirName(info.cwd)}
          </span>
        ) : null}
        {info && info.live ? (
          <span className="wr-live">
            <span className="wr-dot" />
            {t("{running} running / {total} stages", { running: info.running, total: info.total })}
          </span>
        ) : info ? (
          <span className="wr-meta">{t("finished · {n} stages", { n: info.total })}</span>
        ) : null}
        {info && info.failed ? <span className="wr-failed">{t("{n} failed", { n: info.failed })}</span> : null}
        <button className="wr-close" onClick={onClose}>
          {t("Close ✕")}
        </button>
      </div>
      {root ? (
        <div className="wr-body">
          <div className="wr-canvas">
            <ReactFlow
              // Remount on a run switch so fitView re-runs — otherwise the incoming graph
              // inherits the previous run's zoom/pan and can land entirely off-screen.
              key={root.run_id}
              nodes={nodes}
              edges={edges}
              nodeTypes={nodeTypes}
              onNodesChange={onNodesChange}
              onEdgesChange={onEdgesChange}
              onNodeClick={(_, n) => setSelId(n.id)}
              onNodeDragStop={(_, n) => pinnedPos.current.set(n.id, n.position)}
              onPaneClick={() => setSelId(null)}
              fitView
              minZoom={0.3}
            >
              <Background color="#232a3a" gap={22} />
              <MiniMap pannable zoomable />
              <Controls showInteractive={false} />
            </ReactFlow>
          </div>
          {selected ? <StageDetail t={t} data={selected.data} onClose={() => setSelId(null)} /> : null}
        </div>
      ) : (
        <div className="wr-empty">{t("No workflow has run yet — start one with `agentpit workflow …`")}</div>
      )}
    </div>
  );
}

const MEMBER_STATE = { ok: "done", error: "failed", running: "running" };

// Why a stage is red, what each backend did, and where it ran. Without this the
// per-member `error` the event log already carries is never visible anywhere.
function StageDetail({ t, data, onClose }) {
  const run = data.run || {};
  const members = run.members || [];
  return (
    <aside className="wr-detail">
      <div className="wr-detail-hd">
        <span className="wr-detail-title">{data.title}</span>
        <button className="wr-detail-close" onClick={onClose} aria-label={t("Close ✕")}>
          ✕
        </button>
      </div>
      <dl className="wr-kv">
        <dt>{t("Status")}</dt>
        <dd className={`st-${data.status}`}>{t(data.status)}</dd>
        <dt>{t("Kind")}</dt>
        <dd>{run.kind || "—"}</dd>
        <dt>{t("Role")}</dt>
        <dd>{run.role || "—"}</dd>
        <dt>{t("Elapsed")}</dt>
        <dd>{elapsed(data) || "—"}</dd>
        <dt>{t("Directory")}</dt>
        <dd className="wr-kv-wrap">{run.cwd || "—"}</dd>
        <dt>{t("Run ID")}</dt>
        <dd className="wr-kv-wrap">{run.run_id || "—"}</dd>
      </dl>
      <div className="wr-detail-sub">{t("Agents")}</div>
      {members.length === 0 ? (
        <div className="wr-detail-empty">{t("No agent has started yet.")}</div>
      ) : (
        <ul className="wr-members">
          {members.map((m, i) => (
            <li key={`${m.backend}-${m.aggregator ? "agg" : "m"}-${i}`} className={`st-${MEMBER_STATE[m.status] || "running"}`}>
              <div className="wr-member-hd">
                <span className="wr-member-be">{m.backend}</span>
                {m.aggregator ? <span className="wr-member-tag">{t("aggregator")}</span> : null}
                <span className="wr-member-st">{m.status}</span>
              </div>
              <div className="wr-member-bd">
                {m.elapsed_ms != null ? <span>{Math.round(m.elapsed_ms / 1000)}s</span> : null}
                {m.chars != null ? <span>{t("{n} chars", { n: m.chars })}</span> : null}
              </div>
              {m.error ? <div className="wr-member-err">{m.error}</div> : null}
            </li>
          ))}
        </ul>
      )}
    </aside>
  );
}
