import { Handle, Position } from "@xyflow/react";

function fmt(secs) {
  return secs < 60 ? `${secs}s` : `${Math.floor(secs / 60)}m ${String(secs % 60).padStart(2, "0")}s`;
}

export function elapsed(data) {
  if (data.finished) {
    return data.durationMs ? fmt(Math.round(data.durationMs / 1000)) : "";
  }
  if (!data.startedTs) return "";
  return `${fmt(Math.max(0, Math.round((Date.now() - data.startedTs) / 1000)))}…`;
}

const STATUS = {
  running: { color: "var(--ac)", text: "var(--ac-tx)", label: "running" },
  done: { color: "#5fb884", text: "#7fce9e", label: "done" },
  failed: { color: "#e5687a", text: "#f08a98", label: "failed" },
};

export default function StageNode({ data, selected }) {
  const s = STATUS[data.status] || STATUS.running;
  const handleStyle = {
    width: 8,
    height: 8,
    background: "var(--panel-2)",
    border: "1.5px solid var(--line-3)",
  };
  return (
    <div
      className={`wr-stage ${data.isRoot ? "root" : ""} st-${data.status} ${selected ? "sel" : ""}`}
      style={{ "--st": s.color, "--st-tx": s.text }}
    >
      <Handle type="target" position={Position.Left} style={handleStyle} />
      <div className="wr-stage-hd">
        <span className={`wr-dot ${data.status === "running" ? "pulse" : ""}`} />
        <span className="wr-stage-title">{data.title}</span>
        <span className="wr-stage-status">{s.label}</span>
      </div>
      <div className="wr-stage-bd">
        {data.subtitle ? <span className="wr-stage-sub">{data.subtitle}</span> : null}
        <span className="wr-stage-elapsed">{elapsed(data)}</span>
      </div>
      <Handle type="source" position={Position.Right} style={handleStyle} />
    </div>
  );
}
