import { useCallback, useEffect, useMemo, useRef, useState } from "react";
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
import { stepNodeData, metaFor, resolveWorker } from "./backends.js";
import { loadBlueprint, saveBlueprint, edgesFor, blueprintKey, workflowName, seedBlueprint, deriveFlow, deriveSteps, hasBlueprint } from "./blueprint.js";
import { draftFromSettings, validate, buildPayload, newRole, newType, typeNameError } from "./settings.js";
import { indexModelCatalogs, primaryRoleCatalog } from "./model-catalogs.js";
import { loadSavedSteps, saveSavedSteps, stepTemplate, stepFromTemplate, maxStepSeq } from "./savedsteps.js";
import { Field, Text, Area, Num, Toggle, Select, TriState, BackendChips } from "./forms.jsx";
import { t as tr, setStudioLang, detectLang, persistLang } from "./i18n.js";
import "../reactflow-dark.css";
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

async function loadModelCatalogs(refresh = false) {
  const invoke = window.__TAURI__?.core?.invoke;
  if (invoke) return await invoke("get_model_catalogs", { refresh });
  return window.__AGENTPIT_MOCK_MODEL_CATALOGS__ || [];
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
  if (currentType == null) return tr("(default)");
  const t = types.find((x) => x._key === currentType);
  return t ? t.name || tr("(unnamed)") : tr("(default)");
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
  nodes.push({ id: "ghost", type: "ghost", position: { x: bp.ghost.x, y: bp.ghost.y }, data: { label: tr("dynamic") } });
  return nodes;
}

export default function StudioApp({ embedded = false }) {
  const bpRef = useRef(loadBlueprint("base"));
  const bpNameRef = useRef("base");
  const builtForRef = useRef(null); // which workflow the current `nodes` were built for
  const rfRef = useRef(null); // ReactFlow instance (for screenToFlowPosition on drop)
  // Seeded from the loaded blueprint's max st-N so a remount/reload can't re-mint
  // an id that already exists; re-seeded per workflow in loadCanvas.
  const stepSeqRef = useRef(maxStepSeq(bpRef.current));
  const [savedSteps, setSavedSteps] = useState(() => loadSavedSteps());
  const [bpVersion, setBpVersion] = useState(0);
  const [wfKey, setWfKey] = useState(0);
  const [draft, setDraft] = useState(null);
  const [currentType, setCurrentType] = useState(null); // null = base [workflow]; else a type _key
  const [dirty, setDirty] = useState(false);
  const [saving, setSaving] = useState(false);
  const [errors, setErrors] = useState({});
  const [saveError, setSaveError] = useState(null);
  const [modelCatalogs, setModelCatalogs] = useState({});
  const [selNodeId, setSelNodeId] = useState(null);
  const [selRoleKey, setSelRoleKey] = useState(null);
  const [gen, setGen] = useState(null); // generate modal: null=closed, else {desc, busy, error}
  const [describingKeys, setDescribingKeys] = useState(() => new Set()); // type _keys whose description is AI-generating
  const [lang, setLang] = useState(detectLang);
  setStudioLang(lang); // sync module translator before children render (idempotent)
  const switchLang = (l) => {
    setLang(persistLang(l)); // persist + notify legacy app.js so the whole dashboard follows
  };
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

  useEffect(() => {
    let alive = true;
    loadModelCatalogs(false)
      .then((catalogs) => {
        if (!alive) return;
        setModelCatalogs(indexModelCatalogs(catalogs));
      })
      .catch(() => {});
    return () => {
      alive = false;
    };
  }, []);

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
      stepSeqRef.current = maxStepSeq(bpRef.current); // per-workflow id counter
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
  const addWorker = (stepId, worker) =>
    updateBlueprint((bp) => ({
      ...bp,
      steps: bp.steps.map((s) => {
        if (s.id !== stepId) return s;
        const ws = s.workers || [];
        if (ws.some((w) => w.type === worker.type && w.id === worker.id)) return s; // no dup
        return { ...s, workers: [...ws, { type: worker.type, id: worker.id }] };
      }),
    }));
  const removeWorker = (stepId, idx) =>
    updateBlueprint((bp) => ({ ...bp, steps: bp.steps.map((s) => (s.id === stepId ? { ...s, workers: (s.workers || []).filter((_, i) => i !== idx) } : s)) }));
  const dropStepTemplate = (tpl, pos) =>
    updateBlueprint((bp) => {
      const step = stepFromTemplate(tpl, pos, (bp.steps || []).length + 1, ++stepSeqRef.current);
      return { ...bp, steps: [...(bp.steps || []), step] };
    });

  // ── saved-step library (global, localStorage) ───────────────────────────────
  const saveStepAsTemplate = (step) => {
    const next = [...savedSteps, stepTemplate(step)];
    setSavedSteps(next);
    saveSavedSteps(next);
  };
  const removeSavedStep = (i) => {
    const next = savedSteps.filter((_, idx) => idx !== i);
    setSavedSteps(next);
    saveSavedSteps(next);
  };

  // ── palette drag-and-drop ───────────────────────────────────────────────────
  const onPaletteDragStart = (payload) => (e) => {
    e.dataTransfer.setData("application/json", JSON.stringify(payload));
    e.dataTransfer.effectAllowed = "copy";
  };
  const onCanvasDragOver = (e) => {
    e.preventDefault();
    e.dataTransfer.dropEffect = "copy";
  };
  const onCanvasDrop = (e) => {
    e.preventDefault();
    let payload;
    try {
      payload = JSON.parse(e.dataTransfer.getData("application/json"));
    } catch {
      return;
    }
    if (!payload) return;
    if (payload.kind === "worker") {
      // must land on a step card
      const nodeEl = e.target.closest(".react-flow__node");
      const nodeId = nodeEl && nodeEl.getAttribute("data-id");
      if (nodeId && (bpRef.current.steps || []).some((s) => s.id === nodeId)) addWorker(nodeId, payload.worker);
    } else if (payload.kind === "savedstep") {
      const pos = rfRef.current ? rfRef.current.screenToFlowPosition({ x: e.clientX, y: e.clientY }) : null;
      dropStepTemplate(payload.template, pos);
    }
  };

  // ── config edits (roles / workflow knobs / types — dirty, saved) ──
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
      // Keep the blueprint namespace in sync with the rename AND migrate the
      // stored sketch to the new key, so the sketch (and its derived flow hint)
      // follows the type instead of orphaning under the old name.
      const newName = workflowName({ _key: key, name: val });
      const oldName = bpNameRef.current;
      if (oldName !== newName) {
        try {
          const raw = localStorage.getItem(blueprintKey(oldName));
          if (raw != null) {
            localStorage.setItem(blueprintKey(newName), raw);
            localStorage.removeItem(blueprintKey(oldName));
          }
        } catch {
          /* best-effort */
        }
        bpNameRef.current = newName;
      }
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
      // Phase 4: recompute the soft flow hint ONLY for workflows the user actually
      // sketched (a real localStorage blueprint). A config-authored type never
      // opened in the canvas keeps its existing flow/plan — we must NOT stamp the
      // generic seed onto it (that would clobber a hand-authored one and break
      // "unset = inherit / no hint"). `flow` and `steps` come from the SAME sketch,
      // so they can never disagree about the step order.
      const derive = (name, existing) => {
        if (!hasBlueprint(name)) return { flow: existing.flow || "", steps: existing.steps || [] };
        const bp = name === bpNameRef.current ? bpRef.current : loadBlueprint(name);
        return { flow: deriveFlow(bp), steps: deriveSteps(bp) };
      };

      const withFlows = {
        ...draft,
        // The BASE canvas gets the same treatment as a named type.
        workflow: { ...draft.workflow, ...derive("base", draft.workflow) },
        types: draft.types.map((t) => ({ ...t, ...derive(workflowName(t), t) })),
      };
      if (invoke) await invoke("settings_save", { payload: buildPayload(withFlows) });
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

  // ── AI generate / describe ──────────────────────────────────────────────────
  // Apply a generated proposal as an UNSAVED draft: merge roles into the cast, add
  // a uniquely-named type, and seed + persist its blueprint sketch. Nothing hits
  // config until the user clicks Save.
  const applyProposal = (p) => {
    if (!p || !p.type) throw new Error("Invalid generation result.");
    // Compute EVERYTHING (incl. the throw-prone step mapping) before any setState,
    // so a malformed proposal can't leave a half-applied draft + orphan type.
    let roles = [...draft.roles];
    for (const pr of p.roles || []) {
      if (!pr || !pr.name) continue;
      const ex = roles.find((r) => r.name === pr.name);
      if (ex) {
        roles = roles.map((r) =>
          r.name === pr.name ? { ...r, backends: r.backends.length ? r.backends : [...(pr.backends || [])], prompt: r.prompt || pr.prompt || "" } : r
        );
      } else {
        roles = [...roles, { ...newRole(), name: pr.name, backends: [...(pr.backends || [])], prompt: pr.prompt || "" }];
      }
    }
    const names = new Set(draft.types.map((t) => t.name));
    let name = p.type;
    for (let n = 2; names.has(name); n++) name = `${p.type}-${n}`;
    const t = {
      ...newType(),
      name,
      title: p.title || "",
      prompt: p.brief || "",
      roles: [...(p.uses_roles || [])],
      manager_backend: p.manager_backend || "",
      max_depth: p.max_depth ?? null,
      max_calls_per_manager: p.max_calls_per_manager ?? null,
      use_mcp: p.use_mcp ?? null,
      enable_ask_human: p.enable_ask_human ?? null,
    };
    const src = p.steps && p.steps.length ? p.steps : seedBlueprint().steps;
    const steps = src.map((s, i) => ({
      id: "st-" + (i + 1),
      index: i + 1 < 10 ? "0" + (i + 1) : "" + (i + 1),
      name: s.name || "step",
      manager: s.manager || p.manager_backend || "claude",
      persona: s.persona || "",
      behavior: s.behavior || "",
      dynamic: s.dynamic !== false,
      ask: !!s.ask,
      fanout: s.fanout || 2,
      // filter falsy (empty string / null) workers before mapping, then drop empty ids
      workers: (s.workers || [])
        .filter(Boolean)
        .map((w) => (typeof w === "string" ? { type: "role", id: w } : { type: (w && w.type) || "role", id: w && w.id }))
        .filter((w) => w.id),
      x: 320 + i * 300,
      y: 200,
      w: 250,
    }));
    const bp = {
      goal: { id: "goal", x: 40, y: 250, w: 210 },
      ghost: { id: "ghost", x: (steps.length ? 320 + (steps.length - 1) * 300 : 320) + 300, y: 236, w: 156 },
      steps,
      gensteps: [],
    };
    // --- all computed and safe; commit ---
    setDraft((d) => ({ ...d, roles, types: [...d.types, t] }));
    setDirty(true);
    saveBlueprint(bpNameRef.current, bpRef.current); // persist OUTGOING first
    bpRef.current = bp;
    bpNameRef.current = workflowName(t);
    stepSeqRef.current = maxStepSeq(bp);
    saveBlueprint(bpNameRef.current, bp);
    setCurrentType(t._key);
    setEdges(edgesFor(bp).map(toRfEdge));
    setBpVersion((v) => v + 1);
    setWfKey((k) => k + 1);
    setSelNodeId(null);
    setSelRoleKey(null);
  };
  const runGenerate = async () => {
    const desc = (gen && gen.desc ? gen.desc : "").trim();
    if (!desc) {
      setGen((g) => ({ ...(g || {}), error: "Enter a description." }));
      return;
    }
    setGen((g) => ({ ...(g || {}), busy: true, error: null }));
    try {
      const invoke = window.__TAURI__?.core?.invoke;
      const proposal = invoke ? await invoke("workflow_generate", { description: desc }) : window.__AGENTPIT_MOCK_PROPOSAL__ || null;
      if (!proposal) throw new Error("No backend available to generate.");
      applyProposal(proposal);
      setGen(null);
    } catch (e) {
      setGen((g) => ({ ...(g || {}), busy: false, error: String(e && e.message ? e.message : e) }));
    }
  };
  const runDescribe = async (t) => {
    const snapshot = t.description || "";
    // "none selected = all worker roles" — expand so the describer gets the real cast.
    const usesRoles = t.roles && t.roles.length ? t.roles : draft.roles.map((r) => r.name).filter((n) => n && n !== "manager");
    setDescribingKeys((s) => new Set(s).add(t._key));
    setSaveError(null);
    try {
      const spec = {
        title: t.title,
        manager_backend: t.manager_backend,
        brief: t.prompt,
        roles: usesRoles
          .map((nm) => {
            const r = draft.roles.find((x) => x.name === nm);
            return r ? { name: nm, backends: r.backends || [], prompt: r.prompt || "" } : null;
          })
          .filter(Boolean),
        uses_roles: t.roles || [],
        max_depth: t.max_depth,
        max_calls_per_manager: t.max_calls_per_manager,
        use_mcp: t.use_mcp,
        enable_ask_human: t.enable_ask_human,
        steps: (bpRef.current.steps || []).map((s) => ({ name: s.name, persona: s.persona, behavior: s.behavior })),
      };
      const invoke = window.__TAURI__?.core?.invoke;
      const desc = invoke ? await invoke("workflow_describe", { spec }) : window.__AGENTPIT_MOCK_DESCRIBE__ || "";
      if (typeof desc === "string" && desc.trim()) {
        // Only apply if the user hasn't edited the field meanwhile — keep their edit.
        setDraft((d) => ({
          ...d,
          types: d.types.map((x) => (x._key === t._key && (x.description || "") === snapshot ? { ...x, description: desc.trim() } : x)),
        }));
        setDirty(true);
      } else {
        setSaveError("The model returned an empty description.");
      }
    } catch (e) {
      setSaveError(String(e && e.message ? e.message : e));
    } finally {
      setDescribingKeys((s) => {
        const n = new Set(s);
        n.delete(t._key);
        return n;
      });
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

  // Exactly what Save will write for this workflow: the one-line `flow` hint and the
  // structured `steps` plan. Keyed on bpVersion (bumped by every sketch edit and workflow
  // switch), so editing a card or drawing an arrow shows its effect on config immediately —
  // without this the canvas→config link is invisible and the sketch reads as decoration.
  // Memoized because the whole editor re-renders on unrelated state (typing in the
  // inspector, a model-catalog fetch) and the derivation walks the full edge graph.
  const flowPreview = useMemo(() => deriveFlow(bpRef.current), [bpVersion]);
  const planPreview = useMemo(() => deriveSteps(bpRef.current), [bpVersion]);

  if (!draft) return <div className={"sd-root" + (embedded ? " sd-embedded" : "")} />;

  const backends = draft.known_backends;
  const beOpts = backends.map((b) => ({ value: b, label: metaFor(b).label }));
  const selStep = (bpRef.current.steps || []).find((s) => s.id === selNodeId) || null;
  const selRole = draft.roles.find((r) => r._key === selRoleKey) || null;
  const curType = currentType == null ? null : draft.types.find((t) => t._key === currentType) || null;
  const workerRoleNames = draft.roles.map((r) => r.name).filter((n) => n && n !== "manager");

  return (
    <div className={"sd-root" + (embedded ? " sd-embedded" : "")}>
      <div className="sd-top">
        <span className="sd-badge">{tr("BLUEPRINT")}</span>
        <select className="sd-switch" value={currentType == null ? "base" : "t" + currentType} onChange={onSwitcher}>
          <option value="base">{tr("(default) workflow")}</option>
          {draft.types.map((ty) => (
            <option key={ty._key} value={"t" + ty._key}>
              {ty.title || ty.name || tr("(unnamed)")}
            </option>
          ))}
          <option value="__new">{tr("＋ New workflow")}</option>
        </select>
        <span className="sd-hint">{tr("drag a handle → handle to draw an arrow")}</span>
        <span className={"sd-dirty" + (dirty ? " on" : "")}>{saving ? tr("Saving…") : dirty ? tr("Unsaved config") : tr("Saved")}</span>
        <span className="sd-langs">
          <button className={"sd-lang" + (lang === "en" ? " on" : "")} onClick={() => switchLang("en")}>EN</button>
          <button className={"sd-lang" + (lang === "ja" ? " on" : "")} onClick={() => switchLang("ja")}>日本語</button>
        </span>
        <button className="sd-gen" onClick={() => setGen({ desc: "", busy: false, error: null })}>
          {tr("✨ Generate")}
        </button>
        <button className="sd-save" disabled={!dirty || saving} onClick={save}>
          {tr("Save config")}
        </button>
        {!embedded ? (
          <button className="sd-close" onClick={() => window.__agentpitCloseSettings?.()}>
            {tr("Close ✕")}
          </button>
        ) : null}
      </div>
      {saveError ? <div className="sd-saveerr">{tr(saveError)}</div> : null}
      <div className="sd-body">
        <div className="sd-palette">
          <div className="sd-pal-hd">{tr("PALETTE")}</div>
          <div className="sd-pal-sub">{tr("drag a CLI/role onto a step · a saved step onto the canvas")}</div>

          <div className="sd-pal-sec-hd">
            <span>{tr("CLIs")}</span>
          </div>
          <div className="sd-pal-clis">
            {backends.map((b) => (
              <div key={b} className="sd-pal-cli" draggable onDragStart={onPaletteDragStart({ kind: "worker", worker: { type: "cli", id: b } })} title={tr("drag onto a step")}>
                <span className="sd-mono" style={{ background: metaFor(b).color }}>{metaFor(b).mono}</span>
                {metaFor(b).label}
              </div>
            ))}
          </div>

          <div className="sd-pal-sec-hd">
            <span>{tr("CAST · roles")}</span>
            <button className="sd-mini" onClick={addRole}>＋</button>
          </div>
          {draft.roles.length === 0 ? <div className="sd-empty">{tr("No roles yet.")}</div> : null}
          {draft.roles.map((r) => (
            <div
              key={r._key}
              className={"sd-cast-item" + (r.name ? " drag" : "") + (selRoleKey === r._key ? " sel" : "") + (errors[r._key] ? " err" : "")}
              draggable={!!r.name}
              onDragStart={r.name ? onPaletteDragStart({ kind: "worker", worker: { type: "role", id: r.name } }) : undefined}
              onClick={() => {
                setSelRoleKey(r._key);
                setSelNodeId(null);
              }}
              title={r.name ? tr("click to edit · drag onto a step") : tr("name this role to drag it")}
            >
              <span className="sd-cast-name">{r.name || tr("(unnamed)")}</span>
              <span className="sd-cast-be">
                {(r.backends || []).map((b) => (
                  <span key={b} className="sd-mono" style={{ background: metaFor(b).color }}>{metaFor(b).mono}</span>
                ))}
              </span>
            </div>
          ))}

          <div className="sd-pal-sec-hd">
            <span>{tr("SAVED STEPS")}</span>
          </div>
          {savedSteps.length === 0 ? <div className="sd-empty">{tr("Save a step (in its inspector) → drag it onto any canvas.")}</div> : null}
          {savedSteps.map((s, i) => (
            <div key={i} className="sd-saved-item" draggable onDragStart={onPaletteDragStart({ kind: "savedstep", template: s })} title={tr("drag onto the canvas")}>
              <span className="sd-cast-name">{s.name || tr("(step)")}</span>
              <button className="sd-x" onClick={() => removeSavedStep(i)} title={tr("remove template")}>✕</button>
            </div>
          ))}
        </div>

        <div className="sd-canvas" onDrop={onCanvasDrop} onDragOver={onCanvasDragOver}>
          <ReactFlow
            key={wfKey}
            onInit={(inst) => (rfRef.current = inst)}
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
            <RoleForm
              role={selRole}
              backends={backends}
              modelCatalog={primaryRoleCatalog(modelCatalogs, selRole.backends)}
              error={errors[selRole._key]}
              onField={setRoleField}
              onToggleBackend={toggleRoleBackend}
              onRemove={removeRole}
            />
          ) : selStep ? (
            <StepForm step={selStep} beOpts={beOpts} roles={draft.roles} onField={setStepField} onDelete={deleteStep} onRemoveWorker={removeWorker} onSaveTemplate={saveStepAsTemplate} />
          ) : curType ? (
            <TypeForm
              type={curType}
              beOpts={beOpts}
              roleNames={workerRoleNames}
              error={errors["t" + curType._key]}
              describing={describingKeys.has(curType._key)}
              onField={setTypeField}
              onToggleRole={toggleTypeRole}
              onDescribe={runDescribe}
              onDelete={deleteType}
              flowPreview={flowPreview}
              planPreview={planPreview}
            />
          ) : (
            <WorkflowForm
              wf={draft.workflow}
              backends={backends}
              beOpts={beOpts}
              onField={setWorkflowField}
              flowPreview={flowPreview}
              planPreview={planPreview}
            />
          )}
        </div>
      </div>
      {gen ? <GenerateModal gen={gen} setGen={setGen} onGenerate={runGenerate} /> : null}
    </div>
  );
}

function GenerateModal({ gen, setGen, onGenerate }) {
  return (
    <div className="sd-modal-overlay" onClick={(e) => e.target === e.currentTarget && !gen.busy && setGen(null)}>
      <div className="sd-modal">
        <div className="sd-eyebrow">{tr("✨ GENERATE")}</div>
        <h3 className="sd-modal-title">{tr("Generate a workflow")}</h3>
        <div className="sd-note">{tr("Describe the workflow in plain language. An agent drafts the cast (roles) and an illustrative blueprint — nothing is saved until you review and Save.")}</div>
        <textarea
          className="sd-input sd-area"
          rows={4}
          autoFocus
          placeholder={tr("e.g. A workflow that strictly reviews PRs and hardens security & edge cases with refutation")}
          value={gen.desc}
          disabled={gen.busy}
          onChange={(e) => setGen((g) => ({ ...g, desc: e.target.value }))}
        />
        {gen.error ? <div className="sd-fielderr">{tr(gen.error)}</div> : null}
        <div className="sd-modal-actions">
          <button className="sd-close" disabled={gen.busy} onClick={() => setGen(null)}>
            {tr("Cancel")}
          </button>
          <button className="sd-save" disabled={gen.busy} onClick={onGenerate}>
            {gen.busy ? tr("Generating…") : tr("Generate")}
          </button>
        </div>
      </div>
    </div>
  );
}

function StepForm({ step, beOpts, roles, onField, onDelete, onRemoveWorker, onSaveTemplate }) {
  const workers = step.workers || [];
  return (
    <>
      <h3>
        {step.index} · {step.name || "(step)"}
      </h3>
      <Field label={tr("Name")}>
        <Text value={step.name} onChange={(v) => onField(step.id, "name", v)} />
      </Field>
      <Field label={tr("Manager")}>
        <Select value={step.manager} onChange={(v) => onField(step.id, "manager", v)} options={beOpts} placeholder={tr("pick a backend")} />
      </Field>
      <Field label={tr("Persona")}>
        <Area value={step.persona} onChange={(v) => onField(step.id, "persona", v)} rows={2} />
      </Field>
      <Field label={tr("Behavior / directive")}>
        <Area value={step.behavior} onChange={(v) => onField(step.id, "behavior", v)} rows={3} />
      </Field>
      <div className="sd-togglerow">
        <Toggle checked={!!step.dynamic} onChange={(v) => onField(step.id, "dynamic", v)} label={tr("self-spawn")} />
        <Toggle checked={!!step.ask} onChange={(v) => onField(step.id, "ask", v)} label={tr("ask human")} />
      </div>
      <Field label={tr("Workers")}>
        <div className="sd-chips">
          {workers.map((w, i) => {
            const rw = resolveWorker(w, roles);
            return (
              <span key={i} className="sd-wchip" style={{ borderColor: rw.color }}>
                {rw.mono ? (
                  <span className="sd-mono" style={{ background: rw.color }}>{rw.mono}</span>
                ) : (
                  <span className="sd-swatch" style={{ background: rw.color }} />
                )}
                {rw.label}
                <button className="sd-x" onClick={() => onRemoveWorker(step.id, i)} title={tr("remove")}>✕</button>
              </span>
            );
          })}
          {workers.length === 0 ? <span className="sd-empty">{tr("drag a CLI/role from the palette onto this card")}</span> : null}
        </div>
      </Field>
      <button className="sd-mini-btn" onClick={() => onSaveTemplate(step)}>
        {tr("Save as template")}
      </button>
      <button className="sd-danger" onClick={() => onDelete(step.id)}>
        {tr("Delete step")}
      </button>
      <div className="sd-note">{tr("Step cards are the blueprint sketch (saved locally), not config.")}</div>
    </>
  );
}

// What the canvas will actually write to config: the ordered plan (one entry per step card)
// and the one-line flow it distils to. Shown in the inspector so the canvas→config link is
// visible while sketching — it is a HINT the manager adapts, never a DAG it must follow.
function FlowPreview({ flow, plan }) {
  return (
    <Field label={tr("Plan written to config (from your cards and arrows)")}>
      {plan && plan.length ? (
        <>
          <ol className="sd-plan">
            {plan.map((s, i) => {
              const workers = [...s.roles, ...s.backends];
              return (
                <li key={`${s.name}-${i}`}>
                  <span className="sd-plan-name">{s.name}</span>
                  {s.manager_backend ? <span className="sd-plan-meta">{metaFor(s.manager_backend).label}</span> : null}
                  {workers.length ? <span className="sd-plan-meta">→ {workers.join(", ")}</span> : null}
                </li>
              );
            })}
          </ol>
          <div className="sd-flow">{flow}</div>
        </>
      ) : (
        <div className="sd-note">{tr("No named steps on the canvas yet — nothing will be written.")}</div>
      )}
      <div className="sd-note">{tr("Injected into the manager brief as a non-binding suggestion.")}</div>
    </Field>
  );
}

function WorkflowForm({ wf, backends, beOpts, onField, flowPreview, planPreview }) {
  return (
    <>
      <h3>{tr("WORKFLOW · base [workflow]")}</h3>
      <div className="sd-note">{tr("Invoke:")} <code>agentpit workflow "&lt;goal&gt;"</code></div>
      <Field label={tr("Manager backend")}>
        <Select value={wf.manager_backend} onChange={(v) => onField("manager_backend", v || "")} options={beOpts} placeholder={tr("default backend")} />
      </Field>
      <Field label={tr("Default worker agents")}>
        <BackendChips
          selected={wf.default_agents || []}
          all={backends}
          onToggle={(backend) => {
            const selected = wf.default_agents || [];
            onField("default_agents", selected.includes(backend) ? selected.filter((item) => item !== backend) : [...selected, backend]);
          }}
        />
        <div className="sd-note">{tr("None selected uses every available backend except the manager.")}</div>
      </Field>
      <div className="sd-togglerow">
        <Field label={tr("Max depth")}>
          <Num value={wf.max_depth} min={1} onChange={(v) => onField("max_depth", v)} />
        </Field>
        <Field label={tr("Max calls / manager")}>
          <Num value={wf.max_calls_per_manager} min={1} onChange={(v) => onField("max_calls_per_manager", v)} />
        </Field>
      </div>
      <div className="sd-togglerow">
        <Toggle checked={!!wf.use_mcp} onChange={(v) => onField("use_mcp", v)} label={tr("use MCP")} />
        <Toggle checked={!!wf.enable_ask_human} onChange={(v) => onField("enable_ask_human", v)} label={tr("ask-human")} />
      </div>
      <FlowPreview flow={flowPreview} plan={planPreview} />
      <div className="sd-note">{tr("Backend transport and default models are managed in Settings → Backends. Role models here still override those defaults.")}</div>
      <div className="sd-note">{tr("Saved to config.toml `[workflow]` on Save.")}</div>
    </>
  );
}

function TypeForm({ type, beOpts, roleNames, error, describing, onField, onToggleRole, onDescribe, onDelete, flowPreview, planPreview }) {
  const t = type;
  return (
    <>
      <h3>{tr("WORKFLOW TYPE ·")} {t.title || t.name || tr("(unnamed)")}</h3>
      <Field label={tr("Workflow name")}>
        <Text value={t.name} onChange={(v) => onField(t._key, "name", v.trim())} mono />
      </Field>
      {error ? <div className="sd-fielderr">{tr(error)}</div> : null}
      <div className="sd-note">
        {tr("Invoke:")} <code>agentpit workflow {t.name || "&lt;name&gt;"} "&lt;goal&gt;"</code>
      </div>
      <Field label={tr("Display name (optional)")}>
        <Text value={t.title} onChange={(v) => onField(t._key, "title", v)} placeholder={tr("Strict code review")} />
      </Field>
      <Field label={tr("Brief (manager instruction for this workflow)")}>
        <Area value={t.prompt} onChange={(v) => onField(t._key, "prompt", v)} rows={3} />
      </Field>
      <Field label={tr("Description (when to use)")}>
        <Area value={t.description} onChange={(v) => onField(t._key, "description", v)} rows={2} />
        <button className="sd-ai" disabled={describing} onClick={() => onDescribe(t)}>
          {describing ? tr("✨ Generating…") : tr("✨ Generate with AI")}
        </button>
      </Field>
      <Field label={tr("Roles used (none selected = all worker roles)")}>
        {roleNames.length === 0 ? (
          <div className="sd-empty">{tr("Add roles (the cast) first.")}</div>
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
      <FlowPreview flow={flowPreview} plan={planPreview} />
      <div className="sd-note">{tr("Overrides below — empty = inherit base [workflow].")}</div>
      <Field label={tr("Manager backend (override)")}>
        <Select value={t.manager_backend} onChange={(v) => onField(t._key, "manager_backend", v || "")} options={beOpts} placeholder={tr("inherit")} />
      </Field>
      <div className="sd-togglerow">
        <Field label={tr("Max depth")}>
          <Num value={t.max_depth} min={1} placeholder={tr("inherit")} onChange={(v) => onField(t._key, "max_depth", v)} />
        </Field>
        <Field label={tr("Max calls / manager")}>
          <Num value={t.max_calls_per_manager} min={1} placeholder={tr("inherit")} onChange={(v) => onField(t._key, "max_calls_per_manager", v)} />
        </Field>
      </div>
      <div className="sd-togglerow">
        <Field label={tr("Via MCP")}>
          <TriState value={t.use_mcp} onChange={(v) => onField(t._key, "use_mcp", v)} />
        </Field>
        <Field label={tr("Ask a human")}>
          <TriState value={t.enable_ask_human} onChange={(v) => onField(t._key, "enable_ask_human", v)} />
        </Field>
      </div>
      <button className="sd-danger" onClick={() => onDelete(t._key)}>
        {tr("Delete this workflow")}
      </button>
      <div className="sd-note">{tr("Saved to config.toml `[workflow.types.*]` on Save.")}</div>
    </>
  );
}

function RoleForm({ role, backends, modelCatalog, error, onField, onToggleBackend, onRemove }) {
  return (
    <>
      <h3>{tr("ROLE ·")} {role.name || tr("(unnamed)")}</h3>
      <Field label={tr("Name")}>
        <Text value={role.name} onChange={(v) => onField(role._key, "name", v)} mono />
      </Field>
      {error ? <div className="sd-fielderr">{tr(error)}</div> : null}
      <Field label={tr("Backends (preference order)")}>
        <BackendChips selected={role.backends} all={backends} onToggle={(b) => onToggleBackend(role._key, b)} />
      </Field>
      <Field label={tr("Persona (prompt)")}>
        <Area value={role.prompt} onChange={(v) => onField(role._key, "prompt", v)} rows={3} />
      </Field>
      <Field label={tr("Model (optional)")} mono>
        <Text
          value={role.model}
          onChange={(v) => onField(role._key, "model", v)}
          placeholder={tr("e.g. opus / gpt-5-codex")}
          options={modelCatalog?.models || []}
          mono
        />
        {modelCatalog?.models?.length ? (
          <div className="sd-model-meta">
            {tr("Candidates use the first backend in the preference order: {backend}.", { backend: role.backends[0] })}
          </div>
        ) : null}
      </Field>
      <button className="sd-danger" onClick={() => onRemove(role._key)}>
        {tr("Remove role")}
      </button>
      <div className="sd-note">{tr("Saved to config.toml `[workflow.roles.*]` on Save.")}</div>
    </>
  );
}
