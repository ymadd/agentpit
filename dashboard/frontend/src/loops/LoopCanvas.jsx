import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { ReactFlow, Background, Controls, MiniMap, useNodesState, useEdgesState } from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import "../reactflow-dark.css";
import * as api from "./bridge.js";
import {
  toFlow,
  addNode,
  removeNode,
  updateNode,
  updateDoc,
  connect,
  removeEdge,
  setEdgeOn,
  moveNode,
  diagnosticsByNode,
  fileRefOf,
  NODE_KINDS,
} from "./canvas.js";
import { applyChunks, applyPage } from "./output.js";
import { stageText, budgetUse, duration, gateOptions, shortId, stateOf } from "./format.js";
import { nodeTypes } from "./nodes.jsx";
import { NodeInspector, EdgeInspector, DocInspector } from "./Inspector.jsx";
import StartDialog from "./StartDialog.jsx";

// The blueprint canvas with its two modes (design §12, P3): 設計 edits a blueprint file;
// 実行 shows a loop — its frozen blueprint with each node's state, iteration badges,
// assignee chips and the running step's output. One canvas, one toggle.

export default function LoopCanvas({ t, initial, rows, onClose, onOpenLoop }) {
  const [mode, setMode] = useState(initial.mode);
  const [loopId, setLoopId] = useState(initial.mode === "run" ? initial.loopId : null);
  const [fileRef, setFileRef] = useState(initial.mode === "design" ? initial.ref : null);
  // The frozen copy of a loop whose file is not editable here (inline, or elsewhere).
  const [frozen, setFrozen] = useState(null);
  const [bpName, setBpName] = useState(initial.mode === "design" ? initial.ref.name : null);

  useEffect(() => {
    if (initial.mode === "run") {
      setMode("run");
      setLoopId(initial.loopId);
    }
  }, [initial]);

  // Loops started from this blueprint, newest first (for 実行 from the design side).
  const loopsOfBlueprint = useMemo(
    () =>
      (rows || [])
        .filter((r) => bpName && r.summary.blueprint.name === bpName)
        .sort((a, b) => (b.summary.updated_ts || 0) - (a.summary.updated_ts || 0)),
    [rows, bpName]
  );

  const toRun = () => {
    if (!loopId && loopsOfBlueprint[0]) setLoopId(loopsOfBlueprint[0].summary.loop_id);
    setMode("run");
  };

  return (
    <div className="lp-canvas-wrap">
      <div className="lp-canvas-head">
        <div className="lp-seg">
          <button className={mode === "design" ? "on" : ""} disabled={!fileRef && !frozen} onClick={() => setMode("design")}>
            {t("Design")}
          </button>
          <button className={mode === "run" ? "on" : ""} onClick={toRun}>
            {t("Run")}
          </button>
        </div>
        <span className="lp-title">{bpName || ""}</span>
        {mode === "run" && loopsOfBlueprint.length > 1 ? (
          <select className="lp-input lp-pick" value={loopId || ""} onChange={(e) => setLoopId(e.target.value)}>
            {loopsOfBlueprint.map((r) => (
              <option key={r.summary.loop_id} value={r.summary.loop_id}>
                {shortId(r.summary.loop_id)} · {r.summary.title} · {t(stateOf(r))}
              </option>
            ))}
          </select>
        ) : null}
        <button className="lp-close" onClick={onClose}>
          {t("Close ✕")}
        </button>
      </div>
      {mode === "run" ? (
        loopId ? (
          <RunView
            key={loopId}
            t={t}
            loopId={loopId}
            onBlueprint={(view) => {
              setBpName(view.summary.blueprint.name);
              const ref = fileRefOf(view.source);
              if (ref && !fileRef) setFileRef(ref);
              if (!ref) setFrozen(view.blueprint || null);
            }}
          />
        ) : (
          <div className="lp-empty">{t("No loop of this blueprint yet — Start it from the design side.")}</div>
        )
      ) : fileRef ? (
        <DesignView
          key={`${fileRef.scope}:${fileRef.name}:${fileRef.project || ""}`}
          t={t}
          fileRef={fileRef}
          onName={setBpName}
          onStarted={(id) => {
            setLoopId(id);
            setMode("run");
            onOpenLoop?.(id);
          }}
        />
      ) : frozen ? (
        <FrozenView t={t} doc={frozen} />
      ) : null}
    </div>
  );
}

// ── Run ───────────────────────────────────────────────────────────────────────────────

function RunView({ t, loopId, onBlueprint }) {
  const [view, setView] = useState(null);
  const [err, setErr] = useState(null);
  const [selected, setSelected] = useState(null);
  const [follow, setFollow] = useState(true);
  const [tails, setTails] = useState({});
  const [nodes, setNodes, onNodesChange] = useNodesState([]);
  const [edges, setEdges, onEdgesChange] = useEdgesState([]);
  const told = useRef(false);

  useEffect(() => {
    let alive = true;
    const offView = api.on("loops:view", (p) => {
      if (p.loop_id === loopId) setView(p.view);
    });
    const offChunks = api.on("loops:chunks", (p) => {
      if (p.loop_id === loopId) setTails((cur) => applyChunks(cur, p.chunks));
    });
    api
      .openLoop(loopId)
      .then((v) => alive && setView(v))
      .catch((e) => alive && setErr(e.message));
    return () => {
      alive = false;
      offView();
      offChunks();
      api.closeLoop(loopId).catch(() => {});
    };
  }, [loopId]);

  useEffect(() => {
    if (view && !told.current) {
      told.current = true;
      onBlueprint(view);
    }
  }, [view, onBlueprint]);

  // Follow the action until the user picks a node: the waiting gate, else the running step.
  const autoSelected = useMemo(() => {
    if (!view) return null;
    const waiting = view.nodes.find((n) => n.phase === "waiting");
    if (waiting) return waiting.node;
    const running = view.nodes.find((n) => n.phase === "running" && n.kind !== "repeat");
    return running ? running.node : null;
  }, [view]);
  const sel = follow ? autoSelected || selected : selected;

  useEffect(() => {
    if (!view?.blueprint) return;
    const g = toFlow(view.blueprint, view, sel);
    setNodes(g.nodes);
    setEdges(g.edges);
  }, [view, sel, setNodes, setEdges]);

  const nodeView = view?.nodes.find((n) => n.node === sel) || null;
  const stepId = nodeView?.step_id || null;
  const stepKind = nodeView?.kind;

  // The selected step's output: the tail from disk, then live chunks. A gap re-reads.
  const gap = stepId ? tails[stepId]?.gap : false;
  useEffect(() => {
    if (!stepId || (stepKind !== "agent" && stepKind !== "check" && stepKind !== "manager")) return;
    let alive = true;
    api
      .stepOutput(loopId, stepId, stepKind === "check" ? "check_log" : "output", null, 16384)
      .then((page) => alive && setTails((cur) => applyPage(cur, stepId, page)))
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, [loopId, stepId, stepKind, gap]);

  if (err) return <div className="lp-banner">{err}</div>;
  if (!view) return <div className="lp-empty">{t("Loading…")}</div>;
  return (
    <div className="lp-canvas-body">
      <div className="lp-canvas">
        <ReactFlow
          nodes={nodes}
          edges={edges}
          nodeTypes={nodeTypes}
          onNodesChange={onNodesChange}
          onEdgesChange={onEdgesChange}
          nodesDraggable={false}
          nodesConnectable={false}
          onNodeClick={(_, n) => {
            setSelected(n.id);
            setFollow(false);
          }}
          onPaneClick={() => {
            setSelected(null);
            setFollow(true);
          }}
          fitView
          minZoom={0.3}
        >
          <Background color="#232a3a" gap={22} />
          <MiniMap pannable zoomable />
          <Controls showInteractive={false} />
        </ReactFlow>
      </div>
      <RunSide t={t} view={view} loopId={loopId} node={nodeView} tail={stepId ? tails[stepId] : null} follow={follow} />
    </div>
  );
}

function RunSide({ t, view, loopId, node, tail, follow }) {
  const s = view.summary;
  const budget = budgetUse(s);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState(null);
  const [text, setText] = useState("");
  const op = async (body) => {
    setBusy(true);
    setErr(null);
    try {
      await api.control(loopId, body, api.newOpId());
      return true;
    } catch (e) {
      setErr(e.message);
      return false;
    } finally {
      setBusy(false);
    }
  };
  const terminal = ["succeeded", "failed", "cancelled"].includes(s.status);
  const steps = node ? view.steps.filter((x) => x.node === node.node) : [];
  const gate = node?.gate_id ? view.gates.find((g) => g.gate_id === node.gate_id) : null;
  const tailRef = useRef(null);
  useEffect(() => {
    if (tailRef.current) tailRef.current.scrollTop = tailRef.current.scrollHeight;
  }, [tail?.text]);

  return (
    <aside className="lp-side">
      <div className="lp-side-hd">
        <span className={`lp-state lp-state-${s.status}`}>{t(s.waiting ? "waiting for you" : s.status)}</span>
        <span className="lp-side-title">{s.title}</span>
      </div>
      <div className="lp-muted lp-mono">{view.source?.cwd}</div>
      <div className="lp-row-stage">{stageText(s) || "—"}</div>
      <div className="lp-side-budget">
        <span>{t("steps {v}", { v: budget.stepsText })}</span>
        <span>{t("compute {v}", { v: budget.timeText })}</span>
      </div>
      {view.read_only ? <div className="lp-banner">{t("Read-only: {why}", { why: view.read_only })}</div> : null}
      {!terminal && !view.read_only ? (
        <div className="lp-side-ctl">
          {s.status === "created" ? (
            <button className="lp-btn pri" disabled={busy} onClick={() => op({ op: "start" })}>
              {t("Start")}
            </button>
          ) : s.pause || s.status === "paused" ? (
            <button className="lp-btn" disabled={busy} onClick={() => op({ op: "resume" })}>
              {t("Resume")}
            </button>
          ) : (
            <button className="lp-btn" disabled={busy} onClick={() => op({ op: "pause", mode: "drain" })}>
              {t("Pause")}
            </button>
          )}
          <button
            className="lp-btn"
            disabled={busy}
            onClick={() => window.confirm(t("Stop this loop? Running steps finish first.")) && op({ op: "stop", mode: "graceful" })}
          >
            {t("Stop")}
          </button>
          <button
            className="lp-btn ghost"
            disabled={busy}
            onClick={() => window.confirm(t("Stop now, cancelling running steps?")) && op({ op: "stop", mode: "cancel" })}
          >
            {t("Stop now")}
          </button>
        </div>
      ) : null}
      {!terminal && !view.read_only ? (
        <div className="lp-instruct">
          <textarea
            className="lp-input"
            rows={2}
            placeholder={
              node && node.kind === "agent"
                ? t("Instruction for {node}'s next step…", { node: node.node })
                : t("Instruction for the next agent step…")
            }
            value={text}
            onChange={(e) => setText(e.target.value)}
          />
          <button
            className="lp-btn"
            disabled={busy || !text.trim()}
            onClick={async () => {
              const target = node && node.kind === "agent" ? node.node : undefined;
              if (await op({ op: "instruct", text: text.trim(), ...(target ? { target } : {}) })) setText("");
            }}
          >
            {t("Send")}
          </button>
        </div>
      ) : null}
      {err ? <div className="lp-err">{err}</div> : null}

      {node ? (
        <div className="lp-side-node">
          <div className="lp-sub">
            {node.node}
            {follow ? <span className="lp-muted"> · {t("following")}</span> : null}
          </div>
          <dl className="lp-kv">
            <dt>{t("phase")}</dt>
            <dd>{node.phase}{node.outcome ? ` · ${node.outcome}` : ""}</dd>
            {node.assignee ? (
              <>
                <dt>{t("assignee")}</dt>
                <dd>
                  {[node.assignee.role, node.assignee.backend, node.assignee.model].filter(Boolean).join(" · ")}
                </dd>
              </>
            ) : null}
            {node.iter?.length ? (
              <>
                <dt>{t("iteration")}</dt>
                <dd>{node.iter.join(".")}</dd>
              </>
            ) : null}
            {node.iteration ? (
              <>
                <dt>{t("iterations")}</dt>
                <dd>
                  {node.iteration.n}/{node.iteration.max}
                </dd>
              </>
            ) : null}
          </dl>
          {gate && gate.status === "open" ? <GateAnswer t={t} loopId={loopId} gate={gate} /> : null}
          {node.phase === "running" && node.step_id && node.kind !== "gate" && node.kind !== "repeat" ? (
            <button
              className="lp-btn ghost"
              disabled={busy}
              onClick={() => op({ op: "cancel_step", step_id: node.step_id })}
            >
              {t("Cancel this step")}
            </button>
          ) : null}
          {node.error ? <div className="lp-err">{node.error}</div> : null}
          {tail ? (
            <pre ref={tailRef} className="lp-tail">
              {tail.truncated ? "…\n" : ""}
              {tail.text}
            </pre>
          ) : node.excerpt ? (
            <pre className="lp-tail">{node.excerpt}</pre>
          ) : null}
          {steps.length ? (
            <ul className="lp-steps">
              {steps.map((st) => (
                <li key={st.step_id} className={`lp-step st-${st.status}`}>
                  <code>{st.step_id}</code>
                  <span>{st.outcome || st.status}</span>
                  <span className="lp-muted">{duration(st.elapsed_ms)}</span>
                  {st.cause && st.cause !== "ready" ? <span className="lp-muted">{st.cause}</span> : null}
                </li>
              ))}
            </ul>
          ) : null}
        </div>
      ) : (
        <div className="lp-muted">{t("Click a node to see its steps and output.")}</div>
      )}
      {view.warnings?.length ? (
        <details className="lp-warns">
          <summary>{t("{n} warning(s)", { n: view.warnings.length })}</summary>
          {view.warnings.map((w) => (
            <div key={w.seq}>
              #{w.seq} {w.message}
            </div>
          ))}
        </details>
      ) : null}
    </aside>
  );
}

function GateAnswer({ t, loopId, gate }) {
  const [comment, setComment] = useState("");
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState(null);
  const [opId] = useState(api.newOpId);
  return (
    <div className="lp-card lp-card-gate">
      <div className="lp-card-prompt">{gate.prompt}</div>
      <input
        className="lp-input"
        placeholder={t("Comment (optional, passed to the next step)")}
        value={comment}
        onChange={(e) => setComment(e.target.value)}
      />
      <div className="lp-card-opts">
        {gateOptions(gate).map((o) => (
          <button
            key={o.id}
            disabled={busy}
            className={`lp-opt ${o.outcome === "fail" ? "neg" : o.outcome === "ok" ? "pos" : ""}`}
            onClick={async () => {
              setBusy(true);
              setErr(null);
              try {
                await api.resolveGate(loopId, gate.gate_id, o.id, comment || null, opId);
              } catch (e) {
                setErr(e.message);
                setBusy(false);
              }
            }}
          >
            {t(o.label)}
          </button>
        ))}
      </div>
      {err ? <div className="lp-err">{err}</div> : null}
    </div>
  );
}

// ── Design ────────────────────────────────────────────────────────────────────────────

function DesignView({ t, fileRef, onName, onStarted }) {
  const [entry, setEntry] = useState(null);
  const [doc, setDoc] = useState(null);
  const [savedJson, setSavedJson] = useState(null);
  const [diags, setDiags] = useState([]);
  const [selected, setSelected] = useState(null); // {type:"node"|"edge", id|index}
  const [err, setErr] = useState(null);
  const [saving, setSaving] = useState(false);
  const [conflict, setConflict] = useState(null);
  const [starting, setStarting] = useState(false);
  const [raw, setRaw] = useState(null);
  const [nodes, setNodes, onNodesChange] = useNodesState([]);
  const [edges, setEdges, onEdgesChange] = useEdgesState([]);

  const load = useCallback(async () => {
    setErr(null);
    try {
      const b = await api.getBlueprint(fileRef.scope, fileRef.name, fileRef.project);
      setEntry(b.entry);
      setDoc(b.doc);
      setSavedJson(JSON.stringify(b.doc));
      setDiags(b.diagnostics || []);
      onName?.(b.doc?.name || fileRef.name);
    } catch (e) {
      setErr(e.message);
    }
  }, [fileRef, onName]);

  useEffect(() => {
    load();
  }, [load]);

  // Validate as the document changes (debounced), with the same rules as the CLI.
  useEffect(() => {
    if (!doc) return undefined;
    const id = setTimeout(() => {
      api
        .validateBlueprint(doc)
        .then((v) => setDiags(v.diagnostics || []))
        .catch(() => {});
    }, 300);
    return () => clearTimeout(id);
  }, [doc]);

  const byNode = useMemo(() => (doc ? diagnosticsByNode(doc, diags) : new Map()), [doc, diags]);

  useEffect(() => {
    if (!doc) return;
    const g = toFlow(doc, null, selected?.type === "node" ? selected.id : null);
    for (const n of g.nodes) {
      const d = byNode.get(n.id);
      if (d) n.className = d.some((x) => x.severity === "error") ? "lp-has-error" : "lp-has-warning";
    }
    for (const e of g.edges) e.selected = selected?.type === "edge" && selected.index === e.data.index;
    setNodes(g.nodes);
    setEdges(g.edges);
  }, [doc, selected, byNode, setNodes, setEdges]);

  const dirty = doc && savedJson !== JSON.stringify(doc);

  const save = async (baseRev) => {
    setSaving(true);
    setErr(null);
    try {
      const b = await api.saveBlueprint(fileRef.scope, fileRef.name, doc, baseRev, fileRef.project);
      setEntry(b.entry);
      setSavedJson(JSON.stringify(doc));
      setDiags(b.diagnostics || []);
      setConflict(null);
      return b.entry;
    } catch (e) {
      if (e.code === "conflict") setConflict({ current: e.details?.current || null });
      else setErr(e.message);
      return null;
    } finally {
      setSaving(false);
    }
  };

  const add = (kind) => {
    const parent =
      selected?.type === "node" && doc.nodes.find((n) => n.id === selected.id)?.kind === "repeat" ? selected.id : undefined;
    const { doc: next, id } = addNode(doc, kind, { parent });
    setDoc(next);
    setSelected({ type: "node", id });
  };

  if (err && !doc) return <div className="lp-banner">{err}</div>;
  if (!doc) return <div className="lp-empty">{t("Loading…")}</div>;
  const selNode = selected?.type === "node" ? doc.nodes.find((n) => n.id === selected.id) : null;
  const selEdge = selected?.type === "edge" ? doc.edges?.[selected.index] : null;
  const errors = diags.filter((d) => d.severity === "error").length;

  return (
    <div className="lp-canvas-body">
      <div className="lp-canvas">
        <div className="lp-palette">
          {NODE_KINDS.map((k) => (
            <button key={k} className="lp-btn" onClick={() => add(k)} title={t("Add a {kind} node", { kind: k })}>
              ＋ {k}
            </button>
          ))}
          <span className="lp-sep" />
          <button className="lp-btn" onClick={() => setRaw(raw == null ? JSON.stringify(doc, null, 2) : null)}>
            {raw == null ? t("JSON") : t("Canvas")}
          </button>
          <span className="lp-sep" />
          <span className={`lp-save-state ${dirty ? "dirty" : ""}`}>
            {saving ? t("Saving…") : dirty ? t("Unsaved") : t("Saved")}
          </span>
          <button className="lp-btn pri" disabled={!dirty || saving} onClick={() => save(entry?.rev)}>
            {t("Save")}
          </button>
          <button
            className="lp-btn"
            disabled={saving || errors > 0}
            title={errors ? t("{n} error(s)", { n: errors }) : ""}
            onClick={async () => {
              const ok = dirty ? await save(entry?.rev) : entry;
              if (ok) setStarting(true);
            }}
          >
            {t("Start…")}
          </button>
        </div>
        {raw != null ? (
          <RawEditor
            t={t}
            text={raw}
            onApply={(next) => {
              setDoc(next);
              setRaw(null);
            }}
          />
        ) : (
          <ReactFlow
            nodes={nodes}
            edges={edges}
            nodeTypes={nodeTypes}
            onNodesChange={onNodesChange}
            onEdgesChange={onEdgesChange}
            onNodeClick={(_, n) => setSelected({ type: "node", id: n.id })}
            onEdgeClick={(_, e) => setSelected({ type: "edge", index: e.data.index })}
            onPaneClick={() => setSelected(null)}
            onNodeDragStop={(_, n) => setDoc((d) => moveNode(d, n.id, n.position))}
            onConnect={(c) => setDoc((d) => connect(d, c.source, c.target, "ok"))}
            onNodesDelete={(ns) => setDoc((d) => ns.reduce((acc, n) => removeNode(acc, n.id), d))}
            onEdgesDelete={(es) =>
              setDoc((d) =>
                es
                  .map((e) => e.data.index)
                  .sort((a, b) => b - a)
                  .reduce((acc, i) => removeEdge(acc, i), d)
              )
            }
            deleteKeyCode={["Delete", "Backspace"]}
            fitView
            minZoom={0.3}
          >
            <Background color="#232a3a" gap={22} />
            <MiniMap pannable zoomable />
            <Controls showInteractive={false} />
          </ReactFlow>
        )}
      </div>
      <aside className="lp-side">
        {err ? <div className="lp-err">{err}</div> : null}
        {selNode ? (
          <NodeInspector
            t={t}
            node={selNode}
            diags={byNode.get(selNode.id) || []}
            onChange={(patch) => setDoc((d) => updateNode(d, selNode.id, patch))}
            onDelete={() => {
              setDoc((d) => removeNode(d, selNode.id));
              setSelected(null);
            }}
          />
        ) : selEdge ? (
          <EdgeInspector
            t={t}
            edge={selEdge}
            sourceKind={doc.nodes.find((n) => n.id === selEdge.from)?.kind}
            onChange={(on) => setDoc((d) => setEdgeOn(d, selected.index, on))}
            onDelete={() => {
              setDoc((d) => removeEdge(d, selected.index));
              setSelected(null);
            }}
          />
        ) : (
          <DocInspector t={t} doc={doc} diags={byNode.get("") || []} onChange={(patch) => setDoc((d) => updateDoc(d, patch))} />
        )}
        {diags.length ? (
          <div className="lp-diag-sum">
            {t("{e} error(s), {w} warning(s)", {
              e: errors,
              w: diags.length - errors,
            })}
          </div>
        ) : null}
        <div className="lp-muted lp-mono">{entry?.path}</div>
      </aside>
      {conflict ? (
        <div className="lp-modal-bg">
          <div className="lp-modal">
            <div className="lp-modal-hd">{t("This blueprint changed on disk")}</div>
            <p>
              {conflict.current
                ? t("Someone saved it since you opened it (now {rev}). Your edits are still here.", { rev: conflict.current })
                : t("It was deleted since you opened it. Your edits are still here.")}
            </p>
            <div className="lp-modal-ft">
              <button className="lp-btn" onClick={() => setConflict(null)}>
                {t("Keep editing")}
              </button>
              <button
                className="lp-btn"
                onClick={() => {
                  setConflict(null);
                  load();
                }}
              >
                {t("Discard mine and reload")}
              </button>
              <button className="lp-btn pri" onClick={() => save(conflict.current)}>
                {conflict.current ? t("Overwrite with mine") : t("Save mine again")}
              </button>
            </div>
          </div>
        </div>
      ) : null}
      {starting && entry ? (
        <StartDialog
          t={t}
          entry={entry}
          project={fileRef.project}
          onClose={() => setStarting(false)}
          onStarted={(id) => {
            setStarting(false);
            onStarted(id);
          }}
        />
      ) : null}
    </div>
  );
}

function RawEditor({ t, text, onApply }) {
  const [v, setV] = useState(text);
  const [err, setErr] = useState(null);
  return (
    <div className="lp-raw">
      <textarea className="lp-input mono" value={v} onChange={(e) => setV(e.target.value)} spellCheck={false} />
      {err ? <div className="lp-err">{err}</div> : null}
      <button
        className="lp-btn pri"
        onClick={() => {
          try {
            onApply(JSON.parse(v));
          } catch (e) {
            setErr(String(e.message || e));
          }
        }}
      >
        {t("Apply")}
      </button>
    </div>
  );
}

// A loop's frozen blueprint whose file is not in a blueprint directory: read-only.
function FrozenView({ t, doc }) {
  const [nodes, setNodes, onNodesChange] = useNodesState([]);
  const [edges, setEdges, onEdgesChange] = useEdgesState([]);
  useEffect(() => {
    const g = toFlow(doc, null, null);
    setNodes(g.nodes);
    setEdges(g.edges);
  }, [doc, setNodes, setEdges]);
  return (
    <div className="lp-canvas-body">
      <div className="lp-canvas">
        <div className="lp-note">{t("This loop's blueprint is not a file in a blueprint directory; showing its frozen copy.")}</div>
        <ReactFlow
          nodes={nodes}
          edges={edges}
          nodeTypes={nodeTypes}
          onNodesChange={onNodesChange}
          onEdgesChange={onEdgesChange}
          nodesDraggable={false}
          nodesConnectable={false}
          fitView
        >
          <Background color="#232a3a" gap={22} />
          <Controls showInteractive={false} />
        </ReactFlow>
      </div>
    </div>
  );
}
