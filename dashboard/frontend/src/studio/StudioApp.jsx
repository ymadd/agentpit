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
import { loadBlueprint, saveBlueprint, edgesFor, blueprintKey, workflowName } from "./blueprint.js";
import { draftFromSettings, validate, buildPayload, newRole, newType, typeNameError } from "./settings.js";
import { Field, Text, Area, Num, Toggle, Select, TriState, BackendChips } from "./forms.jsx";
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

function goalLabel(currentType, types) {
  if (currentType == null) return "(default)";
  const t = types.find((x) => x._key === currentType);
  return t ? t.name || "(unnamed)" : "(default)";
}

function buildNodes(bp, roles, label) {
  const nodes = [
    { id: "goal", type: "workflow", position: { x: bp.goal.x, y: bp.goal.y }, data: { label } },
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
  const bpRef = useRef(loadBlueprint("base"));
  const bpNameRef = useRef("base");
  const builtForRef = useRef(null); // which workflow the current `nodes` were built for
  const [bpVersion, setBpVersion] = useState(0);
  const [wfKey, setWfKey] = useState(0);
  const [draft, setDraft] = useState(null);
  const [currentType, setCurrentType] = useState(null); // null = base [workflow]; else a type _key
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
      if (!alive) return;
      const d = draftFromSettings(data);
      setDraft(d);
      setEdges(edgesFor(bpRef.current).map(toRfEdge));
    });
    return () => {
      alive = false;
    };
  }, [setEdges]);

  // Rebuild NODES on any config (role resolution / goal label) or blueprint
  // change, preserving dragged positions. Edges are managed explicitly (initial
  // load, switch, connect, delete) so a workflow switch doesn't keep stale edges.
  useEffect(() => {
    if (!draft) return;
    const label = goalLabel(currentType, draft.types);
    // Keyed by the STABLE workflow id (currentType, not the mutable name) so a
    // rename preserves positions but a switch does not.
    const wfId = currentType == null ? "base" : currentType;
    setNodes((prev) => {
      // Preserve dragged positions only WITHIN a workflow. On a switch, build
      // fresh from the incoming blueprint — every workflow reuses the same node
      // ids, so a cross-workflow position map would stamp the outgoing layout
      // onto the incoming graph (and a later drag would persist it).
      const pos = builtForRef.current === wfId ? new Map(prev.map((n) => [n.id, n.position])) : null;
      builtForRef.current = wfId;
      return buildNodes(bpRef.current, draft.roles, label).map((n) => (pos && pos.has(n.id) ? { ...n, position: pos.get(n.id) } : n));
    });
  }, [draft, currentType, bpVersion, setNodes]);

  // ── workflow switching (per-workflow blueprint) ─────────────────────────────
  const loadCanvas = useCallback(
    (name) => {
      bpRef.current = loadBlueprint(name);
      bpNameRef.current = name;
      setEdges(edgesFor(bpRef.current).map(toRfEdge));
      setBpVersion((v) => v + 1);
      setWfKey((k) => k + 1); // remount ReactFlow → fitView the new graph
      setSelNodeId("goal");
      setSelRoleKey(null);
    },
    [setEdges]
  );
  const switchWorkflow = (typeKey) => {
    saveBlueprint(bpNameRef.current, bpRef.current); // persist OUTGOING first
    const t = typeKey == null ? null : draft.types.find((x) => x._key === typeKey);
    setCurrentType(typeKey);
    loadCanvas(workflowName(t));
  };
  const addType = () => {
    saveBlueprint(bpNameRef.current, bpRef.current);
    const t = newType();
    setDraft((d) => ({ ...d, types: [...d.types, t] }));
    setDirty(true);
    setCurrentType(t._key);
    loadCanvas(workflowName(t));
  };
  const deleteType = (key) => {
    const t = draft.types.find((x) => x._key === key);
    if (t) {
      try {
        localStorage.removeItem(blueprintKey(workflowName(t)));
      } catch {
        /* best-effort */
      }
    }
    setDraft((d) => ({ ...d, types: d.types.filter((x) => x._key !== key) }));
    setDirty(true);
    // NOTE: do NOT saveBlueprint here — the current workflow is the one being
    // deleted, so a save would immediately re-create the sketch we just removed.
    setCurrentType(null);
    loadCanvas("base");
  };
  const onSwitcher = (e) => {
    const v = e.target.value;
    if (v === "__new") addType();
    else switchWorkflow(v === "base" ? null : parseInt(v.slice(1), 10));
  };

  // ── blueprint edits (localStorage sketch — NOT config-dirty) ────────────────
  const updateBlueprint = useCallback((mut) => {
    const next = mut(bpRef.current);
    bpRef.current = next;
    saveBlueprint(bpNameRef.current, next);
    setBpVersion((v) => v + 1);
  }, []);
  const setStepField = (id, key, val) =>
    updateBlueprint((bp) => ({ ...bp, steps: bp.steps.map((s) => (s.id === id ? { ...s, [key]: val } : s)) }));
  const deleteStep = (id) =>
    updateBlueprint((bp) => ({ ...bp, steps: (bp.steps || []).filter((s) => s.id !== id).map((s, i) => ({ ...s, index: i + 1 < 10 ? "0" + (i + 1) : "" + (i + 1) })) }));

  // ── config edits (roles / workflow knobs / types — dirty, saved) ────────────
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
  const setTypeField = (key, field, val) => {
    setDraft((d) => ({ ...d, types: d.types.map((t) => (t._key === key ? { ...t, [field]: val } : t)) }));
    setDirty(true);
    if (field === "name" && key === currentType) {
      // Keep the blueprint namespace in sync with the rename so canvas edits (and
      // the switch-away save) land under the new name, not the stale key.
      bpNameRef.current = workflowName({ _key: key, name: val });
    }
  };
  const toggleTypeRole = (key, roleName) =>
    setDraft((d) => {
      setDirty(true);
      return {
        ...d,
        types: d.types.map((t) =>
          t._key === key ? { ...t, roles: t.roles.includes(roleName) ? t.roles.filter((r) => r !== roleName) : [...t.roles, roleName] } : t
        ),
      };
    });

  const save = async () => {
    const v = validate(draft);
    setErrors(v.errors);
    if (!v.ok) {
      // The offending role/type may be in a workflow that isn't on screen, so
      // surface a banner (the CAST panel + the current inspector also highlight).
      setSaveError("Some roles or workflow types have invalid names (empty, reserved, or duplicate). Fix the highlighted items before saving.");
      return;
    }
    setSaving(true);
    setSaveError(null);
    try {
      const invoke = window.__TAURI__?.core?.invoke;
      if (invoke) await invoke("settings_save", { payload: buildPayload(draft) });
      setDirty(false);
      const data = invoke ? await invoke("settings_get") : null;
      if (data) {
        setDraft(draftFromSettings(data));
        setCurrentType(null);
        loadCanvas("base");
      }
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
    saveBlueprint(bpNameRef.current, bpRef.current);
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
  const curType = currentType == null ? null : draft.types.find((t) => t._key === currentType) || null;
  const workerRoleNames = draft.roles.map((r) => r.name).filter((n) => n && n !== "manager");

  return (
    <div className="sd-root">
      <div className="sd-top">
        <span className="sd-badge">BLUEPRINT</span>
        <select className="sd-switch" value={currentType == null ? "base" : "t" + currentType} onChange={onSwitcher}>
          <option value="base">(default) workflow</option>
          {draft.types.map((t) => (
            <option key={t._key} value={"t" + t._key}>
              {t.title || t.name || "(unnamed)"}
            </option>
          ))}
          <option value="__new">＋ New workflow</option>
        </select>
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
            key={wfKey}
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
          {/* Default (nothing/goal selected) = the workflow settings, base or type. */}
          {selRole ? (
            <RoleForm role={selRole} backends={backends} error={errors[selRole._key]} onField={setRoleField} onToggleBackend={toggleRoleBackend} onRemove={removeRole} />
          ) : selStep ? (
            <StepForm step={selStep} beOpts={beOpts} onField={setStepField} onDelete={deleteStep} />
          ) : curType ? (
            <TypeForm
              type={curType}
              beOpts={beOpts}
              roleNames={workerRoleNames}
              error={errors["t" + curType._key]}
              onField={setTypeField}
              onToggleRole={toggleTypeRole}
              onDelete={deleteType}
            />
          ) : (
            <WorkflowForm wf={draft.workflow} beOpts={beOpts} onField={setWorkflowField} />
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
      <div className="sd-note">Invoke: <code>agentpit workflow "&lt;goal&gt;"</code></div>
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

function TypeForm({ type, beOpts, roleNames, error, onField, onToggleRole, onDelete }) {
  const t = type;
  return (
    <>
      <h3>WORKFLOW TYPE · {t.title || t.name || "(unnamed)"}</h3>
      <Field label="Workflow name">
        <Text value={t.name} onChange={(v) => onField(t._key, "name", v.trim())} mono />
      </Field>
      {error ? <div className="sd-fielderr">{error}</div> : null}
      <div className="sd-note">
        Invoke: <code>agentpit workflow {t.name || "&lt;name&gt;"} "&lt;goal&gt;"</code>
      </div>
      <Field label="Display name (optional)">
        <Text value={t.title} onChange={(v) => onField(t._key, "title", v)} placeholder="Strict code review" />
      </Field>
      <Field label="Brief (manager instruction for this workflow)">
        <Area value={t.prompt} onChange={(v) => onField(t._key, "prompt", v)} rows={3} />
      </Field>
      <Field label="Description (when to use)">
        <Area value={t.description} onChange={(v) => onField(t._key, "description", v)} rows={2} />
      </Field>
      <Field label="Roles used (none selected = all worker roles)">
        {roleNames.length === 0 ? (
          <div className="sd-empty">Add roles (the cast) first.</div>
        ) : (
          <div className="sd-chips">
            {roleNames.map((rn) => (
              <button key={rn} type="button" className={"sd-bchip" + (t.roles.includes(rn) ? " on" : "")} onClick={() => onToggleRole(t._key, rn)}>
                {rn}
              </button>
            ))}
          </div>
        )}
      </Field>
      <div className="sd-note">Overrides below — empty = inherit base [workflow].</div>
      <Field label="Manager backend (override)">
        <Select value={t.manager_backend} onChange={(v) => onField(t._key, "manager_backend", v || "")} options={beOpts} placeholder="inherit" />
      </Field>
      <div className="sd-togglerow">
        <Field label="Max depth">
          <Num value={t.max_depth} min={1} placeholder="inherit" onChange={(v) => onField(t._key, "max_depth", v)} />
        </Field>
        <Field label="Max calls / manager">
          <Num value={t.max_calls_per_manager} min={1} placeholder="inherit" onChange={(v) => onField(t._key, "max_calls_per_manager", v)} />
        </Field>
      </div>
      <div className="sd-togglerow">
        <Field label="Via MCP">
          <TriState value={t.use_mcp} onChange={(v) => onField(t._key, "use_mcp", v)} />
        </Field>
        <Field label="Ask a human">
          <TriState value={t.enable_ask_human} onChange={(v) => onField(t._key, "enable_ask_human", v)} />
        </Field>
      </div>
      <button className="sd-danger" onClick={() => onDelete(t._key)}>
        Delete this workflow
      </button>
      <div className="sd-note">Saved to config.toml `[workflow.types.{t.name || "…"}]` on Save.</div>
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
