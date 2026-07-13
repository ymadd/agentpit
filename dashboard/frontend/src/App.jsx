import { useCallback } from "react";
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

// Phase-1 spike: prove the exact feature the user wants — draw arrows yourself.
// Nodes are draggable (useNodesState) and you can drag from a node's handle to
// another node to CREATE an edge (onConnect → addEdge). This stands in for the
// Studio graph; Phase 2 replaces these with real WORKFLOW/step/genstep nodes.
const initialNodes = [
  { id: "workflow", type: "input", position: { x: 40, y: 160 }, data: { label: "WORKFLOW" } },
  { id: "s1", position: { x: 320, y: 90 }, data: { label: "01 · Plan" } },
  { id: "s2", position: { x: 320, y: 230 }, data: { label: "02 · Review" } },
  { id: "ghost", type: "output", position: { x: 620, y: 160 }, data: { label: "+ dynamic" } },
];

const initialEdges = [
  { id: "e-wf-s1", source: "workflow", target: "s1", markerEnd: { type: MarkerType.ArrowClosed } },
];

export default function App() {
  const [nodes, , onNodesChange] = useNodesState(initialNodes);
  const [edges, setEdges, onEdgesChange] = useEdgesState(initialEdges);

  const onConnect = useCallback(
    (params) =>
      setEdges((eds) =>
        addEdge({ ...params, markerEnd: { type: MarkerType.ArrowClosed } }, eds)
      ),
    [setEdges]
  );

  return (
    <div style={{ width: "100vw", height: "100vh" }}>
      <ReactFlow
        nodes={nodes}
        edges={edges}
        onNodesChange={onNodesChange}
        onEdgesChange={onEdgesChange}
        onConnect={onConnect}
        fitView
      >
        <Background />
        <MiniMap pannable zoomable />
        <Controls />
      </ReactFlow>
    </div>
  );
}
