import { useCallback, useEffect, useRef, useState } from "react";
import {
  ReactFlow,
  Background,
  Controls,
  MiniMap,
  addEdge,
  useNodesState,
  useEdgesState,
  MarkerType,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import StudioStepNode from "../StudioStepNode.jsx";
import { WorkflowNode, GenStepNode, GhostNode } from "./nodes.jsx";
import { stepNodeData, metaFor } from "./backends.js";
import { loadBlueprint, saveBlueprint, edgesFor } from "./blueprint.js";
import { draftFromSettings, validate, roleNameError, buildPayload, newRole } from "./settings.js";
import { Field, Text, Area, Num, Toggle, Select, BackendChips } from "./forms.jsx";
import "./studio.css";

const nodeTypes = {
  workflow: WorkflowNode,
  studioStep: StudioStepNode,
  genstep: GenStepNode,
  ghost: GhostNode,
};

let edgeSeq = 0;

async function loadSettings() {
  const invoke = window.__TAURI__?.core?.invoke;
  if (invoke) {
    try {
      return await invoke("settings_get");
    } catch {
      // fall through
    }
  }
  return window.__AGENTPIT_MOCK_SETTINGS__ || { known_backends: [], roles: [], types: [], workflow: {} };
}

function edgeStyle(kind) {
  switch (kind) {
    case "flow":
      return { stroke: "#7c8595" };
    case "handoff":
      return { stroke: "#8089a0" };
    case "spawn":
    case "subflow":
      return { stroke: "var(--ac)", strokeDasharray: "5 6" };
    case "return":
      return { stroke: "var(--ac-tx)", strokeDasharray: "2 6" };
    case "grow":
      return { stroke: "#4c5667", strokeDasharray: "4 6" };
    default:
      return { stroke: "var(--ac)" };
  }
}

function toRfEdge(e) {
  const kind = e.kind || "custom";
  return {
    id: e.id,
    source: e.source,
    target: e.target,
    data: { kind },
    animated: kind === "spawn" || kind === "subflow",
    markerEnd: { type: MarkerType.ArrowClosed },
    style: edgeStyle(kind),
  };
}

function buildNodes(bp, roles) {
  const nodes = [
    { id: "goal", type: "workflow", position: { x: bp.goal.x, y: bp.goal.y }, data: { label: "(default)" } },
  ];
  for (const st of bp.steps || []) {
    nodes.push({ id: st.id, type: "studioStep", position: { x: st.x, y: st.y }, data: stepNodeData(st, roles) });
  }
  for (const g of bp.gensteps || []) {
    nodes.push({ id: g.id, type: "genstep", position: { x: g.x, y: g.y }, data: { name: g.name, role: g.role, backend: g.backend } });
  }
  nodes.push({ id: "ghost", type: "ghost", position: { x: bp.ghost.x, y: bp.ghost.y }, data: { label: "dynamic" } });
  return nodes;
}

export default function StudioApp() {
  const bpName = "base";
  const bpRef = useRef(loadBlueprint(bpName));
  const [bpVersion, setBpVersion] = useState(0);
  const [draft, setDraft] = useState(null);
  const [dirty, setDirty] = useState(false);
  const [saving, setSaving] = useState(false);
  const [errors, setErrors] = useState({});
  const [saveError, setSaveError] = useState(null);
  const [selNodeId, setSelNodeId] = useState(null);
  const [selRoleKey, setSelRoleKey] = useState(null);
  const [nodes, setNodes, onNodesChange] = useNodesState([]);
  const [edges, setEdges, onEdgesChange] = useEdgesState([]);

  useEffect(() => {
    let alive = true;
    loadSettings().then((data) => {
      if (alive) setDraft(draftFromSettings(data));
    });
    return () => {
      alive = false;
    };
  }, []);

  // (Re)build nodes whenever the config (role resolution) or blueprint changes,
  // preserving any positions the user dragged. Edges are seeded once.
  useEffect(() => {
    if (!draft) return;
    setNodes((prev) => {
      const pos = new Map(prev.map((n) => [n.id, n.position]));
      return buildNodes(bpRef.current, draft.roles).map((n) => (pos.has(n.id) ? { ...n, position: pos.get(n.id) } : n));
    });
    setEdges((prev) => (prev.length ? prev : edgesFor(bpRef.current).map(toRfEdge)));
  }, [draft, bpVersion, setNodes, setEdges]);

  // ── blueprint edits (localStorage sketch — NOT config-dirty) ────────────────
  const updateBlueprint = useCallback((mut) => {
    const next = mut(bpRef.current);
    bpRef.current = next;
    saveBlueprint(bpName, next);
    setBpVersion((v) => v + 1);
  }, []);
  const setStepField = (id, key, val) =>
    updateBlueprint((bp) => ({ ...bp, steps: bp.steps.map((s) => (s.id === id ? { ...s, [key]: val } : s)) }));
  const deleteStep = (id) =>
    updateBlueprint((bp) => ({ ...bp, steps: (bp.steps || []).filter((s) => s.id !== id).map((s, i) => ({ ...s, index: i + 1 < 10 ? "0" + (i + 1) : "" + (i + 1) })) }));

  // ── config edits (roles / workflow knobs — dirty, saved via settings_save) ──
  const setWorkflowField = (key, val) => {
    setDraft((d) => ({ ...d, workflow: { ...d.workflow, [key]: val } }));
    setDirty(true);
  };
  const setRoleField = (key, field, val) => {
    setDraft((d) => ({ ...d, roles: d.roles.map((r) => (r._key === key ? { ...r, [field]: val } : r)) }));
    setDirty(true);
  };
  const toggleRoleBackend = (key, b) =>
    setDraft((d) => {
      setDirty(true);
      return {
        ...d,
        roles: d.roles.map((r) =>
          r._key === key ? { ...r, backends: r.backends.includes(b) ? r.backends.filter((x) => x !== b) : [...r.backends, b] } : r
        ),
      };
    });
  const addRole = () => {
    const r = newRole();
    setDraft((d) => ({ ...d, roles: [...d.roles, r] }));
    setDirty(true);
    setSelRoleKey(r._key);
    setSelNodeId(null);
  };
  const removeRole = (key) => {
    setDraft((d) => ({ ...d, roles: d.roles.filter((r) => r._key !== key) }));
    setDirty(true);
    if (selRoleKey === key) setSelRoleKey(null);
  };

  const save = async () => {
    const v = validate(draft);
    setErrors(v.errors);
    if (!v.ok) return;
    setSaving(true);
    setSaveError(null);
    try {
      const invoke = window.__TAURI__?.core?.invoke;
      if (invoke) await invoke("settings_save", { payload: buildPayload(draft) });
      setDirty(false);
      const data = invoke ? await invoke("settings_get") : null;
      if (data) setDraft(draftFromSettings(data));
    } catch (e) {
      setSaveError(String(e));
    } finally {
      setSaving(false);
    }
  };

  // ── edge drawing / persistence ──────────────────────────────────────────────
  const persistEdges = useCallback((rfEdges) => {
    bpRef.current = {
      ...bpRef.current,
      edges: rfEdges.map((e) => ({ id: e.id, source: e.source, target: e.target, kind: e.data?.kind || "custom" })),
    };
    saveBlueprint(bpName, bpRef.current);
  }, []);
  const onConnect = useCallback(
    (params) => {
      setEdges((eds) => {
        const next = addEdge(
          { ...params, id: `e-${params.source}-${params.target}-${++edgeSeq}`, data: { kind: "custom" }, markerEnd: { type: MarkerType.ArrowClosed }, style: edgeStyle("custom") },
          eds
        );
        persistEdges(next);
        return next;
      });
    },
    [setEdges, persistEdges]
  );
  const onEdgesDelete = useCallback(
    (deleted) => {
      const gone = new Set(deleted.map((e) => e.id));
      setEdges((eds) => {
        const next = eds.filter((e) => !gone.has(e.id));
        persistEdges(next);
        return next;
      });
    },
    [setEdges, persistEdges]
  );
  const onNodeDragStop = useCallback(() => {
    setNodes((ns) => {
      const pos = new Map(ns.map((n) => [n.id, n.position]));
      updateBlueprint((bp) => ({
        ...bp,
        goal: pos.has("goal") ? { ...bp.goal, ...pos.get("goal") } : bp.goal,
        ghost: pos.has("ghost") ? { ...bp.ghost, ...pos.get("ghost") } : bp.ghost,
        steps: (bp.steps || []).map((s) => (pos.has(s.id) ? { ...s, x: pos.get(s.id).x, y: pos.get(s.id).y } : s)),
        gensteps: (bp.gensteps || []).map((g) => (pos.has(g.id) ? { ...g, x: pos.get(g.id).x, y: pos.get(g.id).y } : g)),
      }));
      return ns;
    });
  }, [setNodes, updateBlueprint]);

  if (!draft) return <div className="sd-root" />;

  const backends = draft.known_backends;
  const beOpts = backends.map((b) => ({ value: b, label: metaFor(b).label }));
  const selStep = (bpRef.current.steps || []).find((s) => s.id === selNodeId) || null;
  const selRole = draft.roles.find((r) => r._key === selRoleKey) || null;

  return (
    <div className="sd-root">
      <div className="sd-top">
        <span className="sd-badge">BLUEPRINT</span>
        <span className="sd-name">Workflow Studio</span>
        <span className="sd-hint">drag a handle → handle to draw an arrow</span>
        <span className={"sd-dirty" + (dirty ? " on" : "")}>{saving ? "Saving…" : dirty ? "Unsaved config" : "Saved"}</span>
        <button className="sd-save" disabled={!dirty || saving} onClick={save}>
          Save config
        </button>
        <button className="sd-close" onClick={() => window.__agentpitCloseSettings?.()}>
          Close ✕
        </button>
      </div>
      {saveError ? <div className="sd-saveerr">{saveError}</div> : null}
      <div className="sd-body">
        {/* CAST panel: the roles config.toml owns */}
        <div className="sd-cast">
          <div className="sd-cast-hd">
            <span>CAST · roles</span>
            <button className="sd-mini" onClick={addRole}>
              ＋
            </button>
          </div>
          {draft.roles.length === 0 ? <div className="sd-empty">No roles. Add one → it becomes a `[workflow.roles.*]`.</div> : null}
          {draft.roles.map((r) => (
            <button
              key={r._key}
              className={"sd-cast-item" + (selRoleKey === r._key ? " sel" : "") + (errors[r._key] ? " err" : "")}
              onClick={() => {
                setSelRoleKey(r._key);
                setSelNodeId(null);
              }}
            >
              <span className="sd-cast-name">{r.name || "(unnamed)"}</span>
              <span className="sd-cast-be">
                {(r.backends || []).map((b) => (
                  <span key={b} className="sd-mono" style={{ background: metaFor(b).color }}>
                    {metaFor(b).mono}
                  </span>
                ))}
              </span>
            </button>
          ))}
        </div>

        <div className="sd-canvas">
          <ReactFlow
            nodes={nodes}
            edges={edges}
            nodeTypes={nodeTypes}
            onNodesChange={onNodesChange}
            onEdgesChange={onEdgesChange}
            onConnect={onConnect}
            onEdgesDelete={onEdgesDelete}
            onNodeDragStop={onNodeDragStop}
            onSelectionChange={({ nodes: sel }) => {
              setSelNodeId(sel && sel[0] ? sel[0].id : null);
              if (sel && sel[0]) setSelRoleKey(null);
            }}
            fitView
            minZoom={0.25}
          >
            <Background color="#232a3a" gap={22} />
            <MiniMap pannable zoomable />
            <Controls showInteractive={false} />
          </ReactFlow>
        </div>

        <div className="sd-insp">
          {selRole ? (
            <RoleForm role={selRole} backends={backends} error={errors[selRole._key]} onField={setRoleField} onToggleBackend={toggleRoleBackend} onRemove={removeRole} />
          ) : selStep ? (
            <StepForm step={selStep} beOpts={beOpts} onField={setStepField} onDelete={deleteStep} />
          ) : selNodeId === "goal" ? (
            <WorkflowForm wf={draft.workflow} beOpts={beOpts} onField={setWorkflowField} />
          ) : (
            <div className="sd-empty">Select a step, the WORKFLOW node, or a role to edit it.</div>
          )}
        </div>
      </div>
    </div>
  );
}

function StepForm({ step, beOpts, onField, onDelete }) {
  return (
    <>
      <h3>
        {step.index} · {step.name || "(step)"}
      </h3>
      <Field label="Name">
        <Text value={step.name} onChange={(v) => onField(step.id, "name", v)} />
      </Field>
      <Field label="Manager">
        <Select value={step.manager} onChange={(v) => onField(step.id, "manager", v)} options={beOpts} placeholder="pick a backend" />
      </Field>
      <Field label="Persona">
        <Area value={step.persona} onChange={(v) => onField(step.id, "persona", v)} rows={2} />
      </Field>
      <Field label="Behavior / directive">
        <Area value={step.behavior} onChange={(v) => onField(step.id, "behavior", v)} rows={3} />
      </Field>
      <div className="sd-togglerow">
        <Toggle checked={!!step.dynamic} onChange={(v) => onField(step.id, "dynamic", v)} label="self-spawn" />
        <Toggle checked={!!step.ask} onChange={(v) => onField(step.id, "ask", v)} label="ask human" />
      </div>
      <button className="sd-danger" onClick={() => onDelete(step.id)}>
        Delete step
      </button>
      <div className="sd-note">Step cards are the blueprint sketch (saved locally), not config.</div>
    </>
  );
}

function WorkflowForm({ wf, beOpts, onField }) {
  return (
    <>
      <h3>WORKFLOW · base [workflow]</h3>
      <Field label="Manager backend">
        <Select value={wf.manager_backend} onChange={(v) => onField("manager_backend", v || "")} options={beOpts} placeholder="default backend" />
      </Field>
      <div className="sd-togglerow">
        <Field label="Max depth">
          <Num value={wf.max_depth} min={1} onChange={(v) => onField("max_depth", v)} />
        </Field>
        <Field label="Max calls / manager">
          <Num value={wf.max_calls_per_manager} min={1} onChange={(v) => onField("max_calls_per_manager", v)} />
        </Field>
      </div>
      <div className="sd-togglerow">
        <Toggle checked={!!wf.use_mcp} onChange={(v) => onField("use_mcp", v)} label="use MCP" />
        <Toggle checked={!!wf.enable_ask_human} onChange={(v) => onField("enable_ask_human", v)} label="ask-human" />
      </div>
      <div className="sd-note">Saved to config.toml `[workflow]` on Save.</div>
    </>
  );
}

function RoleForm({ role, backends, error, onField, onToggleBackend, onRemove }) {
  return (
    <>
      <h3>ROLE · {role.name || "(unnamed)"}</h3>
      <Field label="Name">
        <Text value={role.name} onChange={(v) => onField(role._key, "name", v)} mono />
      </Field>
      {error ? <div className="sd-fielderr">{error}</div> : null}
      <Field label="Backends (preference order)">
        <BackendChips selected={role.backends} all={backends} onToggle={(b) => onToggleBackend(role._key, b)} />
      </Field>
      <Field label="Persona (prompt)">
        <Area value={role.prompt} onChange={(v) => onField(role._key, "prompt", v)} rows={3} />
      </Field>
      <Field label="Model (optional)" mono>
        <Text value={role.model} onChange={(v) => onField(role._key, "model", v)} placeholder="e.g. opus / gpt-5-codex" mono />
      </Field>
      <button className="sd-danger" onClick={() => onRemove(role._key)}>
        Remove role
      </button>
      <div className="sd-note">Saved to config.toml `[workflow.roles.*]` on Save.</div>
    </>
  );
}
