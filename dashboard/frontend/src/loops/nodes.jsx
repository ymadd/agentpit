import { memo } from "react";
import { Handle, Position } from "@xyflow/react";

// Canvas nodes for blueprints (design) and loops (run). In run mode `data.run` carries
// the node's state from the LoopView; the colors come from its phase and outcome.

const port = {
  width: 9,
  height: 9,
  background: "var(--panel-2)",
  border: "1.5px solid var(--line-3)",
};

const KIND_ICON = { agent: "◆", check: "✓", gate: "◇", repeat: "↻", manager: "✦" };

export function phaseClass(run) {
  if (!run) return "lp-idle";
  if (run.phase === "done") return `lp-done lp-out-${run.outcome || "ok"}`;
  return `lp-${run.phase}`;
}

function who(assignee) {
  if (!assignee) return null;
  const main = assignee.role || assignee.backend;
  const sub = assignee.role ? assignee.backend : assignee.model;
  return sub ? `${main} · ${sub}` : main;
}

function subtitle(data) {
  if (data.kind === "check") return data.command || "";
  if (data.kind === "gate") return data.prompt || "";
  const cast = data.role || data.backend;
  return cast ? cast : "";
}

export const LoopNode = memo(function LoopNode({ data, selected }) {
  const run = data.run;
  const chip = who(run?.assignee);
  return (
    <div className={`lp-node lp-k-${data.kind} ${phaseClass(run)} ${selected ? "sel" : ""}`}>
      <Handle type="target" position={Position.Left} style={port} />
      <div className="lp-node-hd">
        <span className="lp-node-ic">{KIND_ICON[data.kind] || "•"}</span>
        <span className="lp-node-title">{data.title}</span>
        {run?.phase === "running" ? <span className="lp-pulse" /> : null}
        {run?.phase === "done" && run.outcome ? <span className={`lp-badge lp-out-${run.outcome}`}>{run.outcome}</span> : null}
        {run?.phase === "waiting" ? <span className="lp-badge lp-waiting-b">?</span> : null}
      </div>
      <div className="lp-node-sub">{subtitle(data)}</div>
      <div className="lp-node-ft">
        {chip ? <span className="lp-chip">{chip}</span> : <span className="lp-kind">{data.kind}</span>}
        {run && run.attempts > 1 ? <span className="lp-count">×{run.attempts}</span> : null}
        {run && run.runs > 1 ? <span className="lp-count">#{run.runs}</span> : null}
      </div>
      <Handle type="source" position={Position.Right} style={port} />
    </div>
  );
});

export const LoopGroup = memo(function LoopGroup({ data, selected }) {
  const run = data.run;
  const it = run?.iteration;
  const max = it?.max || data.maxIterations;
  return (
    <div className={`lp-group ${phaseClass(run)} ${selected ? "sel" : ""}`}>
      <Handle type="target" position={Position.Left} style={port} />
      <div className="lp-group-hd">
        <span className="lp-node-ic">↻</span>
        <span className="lp-node-title">{data.title}</span>
        {max ? (
          <span className={`lp-iter ${it?.open ? "open" : ""}`}>
            {it ? it.n : 0}/{max}
          </span>
        ) : null}
        {run?.phase === "done" && run.outcome ? <span className={`lp-badge lp-out-${run.outcome}`}>{run.outcome}</span> : null}
      </div>
      <Handle type="source" position={Position.Right} style={port} />
    </div>
  );
});

export const nodeTypes = { loopNode: LoopNode, loopGroup: LoopGroup };
