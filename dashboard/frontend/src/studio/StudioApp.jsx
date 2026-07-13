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
import { stepNodeData } from "./backends.js";
import { loadBlueprint, saveBlueprint, edgesFor } from "./blueprint.js";
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
  return (
    window.__AGENTPIT_MOCK_SETTINGS__ || {
      known_backends: ["claude", "codex", "gemini", "antigravity", "opencode"],
      roles: [],
      types: [],
      workflow: {},
    }
  );
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
  const [roles, setRoles] = useState([]);
  const [nodes, setNodes, onNodesChange] = useNodesState([]);
  const [edges, setEdges, onEdgesChange] = useEdgesState([]);
  const [selectedId, setSelectedId] = useState(null);

  useEffect(() => {
    let alive = true;
    loadSettings().then((data) => {
      if (!alive) return;
      const rs = data.roles || [];
      setRoles(rs);
      setNodes(buildNodes(bpRef.current, rs));
      setEdges(edgesFor(bpRef.current).map(toRfEdge));
    });
    return () => {
      alive = false;
    };
  }, [setNodes, setEdges]);

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
      const bp = bpRef.current;
      const next = {
        ...bp,
        goal: pos.has("goal") ? { ...bp.goal, ...pos.get("goal") } : bp.goal,
        ghost: pos.has("ghost") ? { ...bp.ghost, ...pos.get("ghost") } : bp.ghost,
        steps: (bp.steps || []).map((s) => (pos.has(s.id) ? { ...s, x: pos.get(s.id).x, y: pos.get(s.id).y } : s)),
        gensteps: (bp.gensteps || []).map((g) => (pos.has(g.id) ? { ...g, x: pos.get(g.id).x, y: pos.get(g.id).y } : g)),
      };
      bpRef.current = next;
      saveBlueprint(bpName, next);
      return ns;
    });
  }, [setNodes]);

  const selectedStep = (bpRef.current.steps || []).find((s) => s.id === selectedId) || null;

  return (
    <div className="sd-root">
      <div className="sd-top">
        <span className="sd-badge">BLUEPRINT</span>
        <span className="sd-name">Workflow Studio</span>
        <span className="sd-hint">drag a handle → handle to draw an arrow · select an edge + Delete to remove</span>
        <button className="sd-close" onClick={() => window.__agentpitCloseSettings?.()}>
          Close ✕
        </button>
      </div>
      <div className="sd-body">
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
            onSelectionChange={({ nodes: sel }) => setSelectedId(sel && sel[0] ? sel[0].id : null)}
            fitView
            minZoom={0.25}
          >
            <Background color="#232a3a" gap={22} />
            <MiniMap pannable zoomable />
            <Controls showInteractive={false} />
          </ReactFlow>
        </div>
        <div className="sd-insp">
          {selectedStep ? (
            <>
              <h3>
                {selectedStep.index} · {selectedStep.name}
              </h3>
              <div className="sd-row">
                <div className="sd-lb">Manager</div>
                <div className="sd-val">{selectedStep.manager}</div>
              </div>
              {selectedStep.persona ? (
                <div className="sd-row">
                  <div className="sd-lb">Persona</div>
                  <div className="sd-val">{selectedStep.persona}</div>
                </div>
              ) : null}
              <div className="sd-row">
                <div className="sd-lb">Behavior / directive</div>
                <div className="sd-val">{selectedStep.behavior || "—"}</div>
              </div>
              <div className="sd-row">
                <div className="sd-lb">Dynamic</div>
                <div className="sd-val">{selectedStep.dynamic ? "self-spawn" : "fixed"}</div>
              </div>
            </>
          ) : (
            <div className="sd-empty">Select a step to inspect it. Editable forms, the palette, and Save land in the next slice.</div>
          )}
        </div>
      </div>
    </div>
  );
}
