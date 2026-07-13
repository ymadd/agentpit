import { useEffect, useMemo, useState } from "react";
import {
  ReactFlow,
  Background,
  Controls,
  MiniMap,
  useNodesState,
  useEdgesState,
} from "@xyflow/react";
import "@xyflow/react/dist/style.css";
import StageNode from "./StageNode.jsx";
import { getSnapshot, pickWorkflow, buildStageGraph, summarize } from "./snapshot.js";
import "./styles.css";

const nodeTypes = { stage: StageNode };
const POLL_MS = 1500;

export default function WorkflowRunApp() {
  const [snapshot, setSnapshot] = useState({ live: [], recent: [] });
  const [open, setOpen] = useState(false);

  useEffect(() => {
    let alive = true;
    const tick = async () => {
      const s = await getSnapshot();
      if (alive) setSnapshot(s || { live: [], recent: [] });
    };
    tick();
    const id = setInterval(tick, POLL_MS);
    return () => {
      alive = false;
      clearInterval(id);
    };
  }, []);

  const root = useMemo(() => pickWorkflow(snapshot), [snapshot]);
  const liveCount = useMemo(
    () => (snapshot.live || []).filter((r) => r.kind === "workflow").length,
    [snapshot]
  );
  const info = useMemo(() => (root ? summarize(snapshot, root) : null), [snapshot, root]);

  return (
    <>
      <button className={`wr-launcher ${liveCount ? "live" : ""}`} onClick={() => setOpen(true)}>
        <span className="wr-dot" />
        <span>Workflow run</span>
        {info ? <span className="wr-count">{info.running ? `${info.running}▸ ` : ""}{info.total} stages</span> : null}
      </button>
      {open && <Overlay snapshot={snapshot} root={root} info={info} onClose={() => setOpen(false)} />}
    </>
  );
}

function Overlay({ snapshot, root, info, onClose }) {
  const [nodes, setNodes, onNodesChange] = useNodesState([]);
  const [edges, setEdges, onEdgesChange] = useEdgesState([]);

  // Rebuild on each poll, preserving any positions the user dragged.
  useEffect(() => {
    if (!root) {
      setNodes([]);
      setEdges([]);
      return;
    }
    const g = buildStageGraph(snapshot, root);
    setNodes((prev) => {
      const pos = new Map(prev.map((n) => [n.id, n.position]));
      return g.nodes.map((n) => (pos.has(n.id) ? { ...n, position: pos.get(n.id) } : n));
    });
    setEdges(g.edges);
  }, [snapshot, root, setNodes, setEdges]);

  return (
    <div className="wr-overlay">
      <div className="wr-head">
        <span className="wr-title">Workflow run</span>
        {info ? (
          <span className="wr-meta">
            {info.manager} · {info.cwd ? info.cwd.split("/").pop() : ""}
          </span>
        ) : null}
        {info && info.live ? (
          <span className="wr-live">
            <span className="wr-dot" />
            {info.running} running / {info.total} stages
          </span>
        ) : info ? (
          <span className="wr-meta">finished · {info.total} stages</span>
        ) : null}
        <button className="wr-close" onClick={onClose}>
          Close ✕
        </button>
      </div>
      {root ? (
        <div className="wr-canvas">
          <ReactFlow
            nodes={nodes}
            edges={edges}
            nodeTypes={nodeTypes}
            onNodesChange={onNodesChange}
            onEdgesChange={onEdgesChange}
            fitView
            minZoom={0.3}
          >
            <Background color="#232a3a" gap={22} />
            <MiniMap pannable zoomable />
            <Controls showInteractive={false} />
          </ReactFlow>
        </div>
      ) : (
        <div className="wr-empty">No workflow has run yet — start one with `agentpit workflow …`</div>
      )}
    </div>
  );
}
