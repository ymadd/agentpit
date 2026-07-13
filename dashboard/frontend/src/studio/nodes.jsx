import { Handle, Position } from "@xyflow/react";
import { metaFor } from "./backends.js";

const port = {
  width: 9,
  height: 9,
  background: "var(--panel-2)",
  border: "1.5px solid var(--line-3)",
};

// The root WORKFLOW node (was the vanilla `.ws-goal`). Shows the workflow name
// (base or a named type).
export function WorkflowNode({ data, selected }) {
  return (
    <div className={`sd-goal ${selected ? "sel" : ""}`}>
      <div className="sd-eyebrow">WORKFLOW</div>
      <div className="sd-goal-txt">{data.label}</div>
      <Handle type="source" position={Position.Right} style={port} />
    </div>
  );
}

// A generated sub-step (the self-spawn swarm): name + role · backend.
export function GenStepNode({ data, selected }) {
  const m = data.backend ? metaFor(data.backend) : null;
  return (
    <div className={`sd-gen ${selected ? "sel" : ""}`}>
      <Handle type="target" position={Position.Top} style={port} />
      <div className="sd-gen-name">{data.name}</div>
      <div className="sd-gen-sub">
        {data.role ? <span className="sd-gen-role">{data.role}</span> : null}
        {m ? (
          <span className="sd-gen-be">
            <span className="sd-mono" style={{ background: m.color }}>{m.mono}</span>
            {m.label}
          </span>
        ) : null}
      </div>
      <Handle type="source" position={Position.Right} style={port} />
    </div>
  );
}

// The "grow" placeholder — the manager can always spawn more at runtime.
export function GhostNode({ data }) {
  return (
    <div className="sd-ghost">
      <Handle type="target" position={Position.Left} style={port} />
      <span className="sd-ghost-plus">＋</span>
      <span>{data.label || "dynamic"}</span>
    </div>
  );
}
