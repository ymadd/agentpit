import React from 'react';
import { Handle, Position } from '@xyflow/react';

function hexA(hex, alpha) {
  const value = String(hex || '').replace(/^#/, '');
  if (!/^[0-9a-fA-F]{6}$/.test(value)) {
    return `rgba(0,0,0,${alpha})`;
  }

  const number = parseInt(value, 16);
  const r = (number >> 16) & 255;
  const g = (number >> 8) & 255;
  const b = number & 255;

  return `rgba(${r},${g},${b},${alpha})`;
}

export default function StudioStepNode({ data, selected }) {
  const nodeData = data || {};
  const workers = Array.isArray(nodeData.workers) ? nodeData.workers : [];
  const hasPersona =
    typeof nodeData.persona === 'string' && nodeData.persona.trim() !== '';

  const handleStyle = {
    width: 9,
    height: 9,
    background: 'var(--panel-2)',
    border: '1.5px solid var(--line-3)',
  };

  return (
    <div className="ws-node">
      <style>{`
        .ws-node {
          --ac:#e0926e;
          --ac-tx:#f0b59a;
          --ac-soft:rgba(224,146,110,.16);
          --ac-bd:rgba(224,146,110,.45);
          --panel:#0a0d14;
          --panel-2:#0c0f17;
          --line-2:#1c2230;
          --line-3:#232a3a;
          --faint:#4c5667;
          --muted:#6b7688;
          --muted-2:#59647a;
          --text:#e6e9ef;
          --text-2:#b8c0cf;
          width:250px;
          border-radius:16px;
          font-family:"IBM Plex Sans JP", sans-serif;
        }

        .ws-node,
        .ws-node * {
          box-sizing:border-box;
        }

        .ws-node .ws-step {
          min-height:222px;
          display:flex;
          flex-direction:column;
          border-radius:16px;
          background:linear-gradient(180deg,#141a27,#10141d);
          border:1px solid var(--line-3);
        }

        .ws-node .ws-step.sel {
          border-color:var(--ac-bd);
        }

        .ws-node .ws-step-hd {
          display:flex;
          align-items:center;
          gap:8px;
          padding:9px 11px;
          cursor:grab;
          background:linear-gradient(
            90deg,
            var(--ac-soft),
            rgba(20,26,39,0) 78%
          );
          border-bottom:1px solid var(--line-2);
          border-radius:15px 15px 0 0;
        }

        .ws-node .ws-idx {
          min-width:24px;
          height:22px;
          padding:0 6px;
          display:grid;
          place-items:center;
          border-radius:6px;
          background:#0b0e16;
          border:1px solid var(--ac-bd);
          color:var(--ac-tx);
          font-family:"IBM Plex Mono", monospace;
          font-size:11px;
          font-weight:600;
        }

        .ws-node .ws-step-name {
          flex:1;
          min-width:0;
          font-size:12.5px;
          font-weight:600;
          color:var(--text);
          overflow:hidden;
          text-overflow:ellipsis;
          white-space:nowrap;
        }

        .ws-node .ws-dyn {
          flex:none;
          font-family:"IBM Plex Mono", monospace;
          font-size:8px;
          letter-spacing:.05em;
          padding:2px 6px;
          border-radius:5px;
        }

        .ws-node .ws-dyn.on {
          background:var(--ac-soft);
          color:var(--ac-tx);
          border:1px solid var(--ac-bd);
        }

        .ws-node .ws-dyn.off {
          background:#10141d;
          color:var(--muted-2);
          border:1px solid var(--line-2);
        }

        .ws-node .ws-step-bd {
          flex:1;
          padding:9px 11px 10px;
          display:flex;
          flex-direction:column;
          gap:7px;
        }

        .ws-node .ws-mgr {
          display:flex;
          align-items:center;
          gap:7px;
        }

        .ws-node .ws-mgr-mono {
          width:20px;
          height:20px;
          flex:none;
          display:grid;
          place-items:center;
          border-radius:5px;
          color:#fff;
          font-family:"IBM Plex Mono", monospace;
          font-size:9px;
        }

        .ws-node .ws-mgr-tag {
          font-family:"IBM Plex Mono", monospace;
          font-size:10px;
          color:var(--muted-2);
        }

        .ws-node .ws-mgr-label {
          flex:1;
          min-width:0;
          font-size:11px;
          color:var(--text-2);
          overflow:hidden;
          text-overflow:ellipsis;
          white-space:nowrap;
        }

        .ws-node .ws-ask {
          flex:none;
          color:var(--ac-tx);
          font-family:"IBM Plex Mono", monospace;
          font-size:9px;
          opacity:.82;
        }

        .ws-node .ws-persona {
          display:-webkit-box;
          overflow:hidden;
          -webkit-box-orient:vertical;
          -webkit-line-clamp:2;
          font-size:10px;
          line-height:1.5;
          color:var(--muted);
        }

        .ws-node .ws-directive {
          padding:6px 8px;
          border-radius:7px;
          background:#0b0e16;
          border:1px solid var(--line-2);
        }

        .ws-node .ws-directive .lb {
          font-family:"IBM Plex Mono", monospace;
          font-size:8px;
          letter-spacing:.1em;
          color:var(--ac-tx);
          margin-bottom:3px;
        }

        .ws-node .ws-directive .bd {
          display:-webkit-box;
          overflow:hidden;
          -webkit-box-orient:vertical;
          -webkit-line-clamp:2;
          font-size:10px;
          line-height:1.5;
          color:var(--text-2);
        }

        .ws-node .ws-workers {
          margin-top:auto;
          display:flex;
          flex-wrap:wrap;
          gap:5px;
          align-items:center;
        }

        .ws-node .ws-chip {
          display:inline-flex;
          align-items:center;
          gap:5px;
          padding:3px 6px;
          border-radius:6px;
        }

        .ws-node .ws-chip-mono {
          width:15px;
          height:15px;
          display:grid;
          place-items:center;
          border-radius:4px;
          color:#fff;
          font-family:"IBM Plex Mono", monospace;
          font-size:8px;
        }

        .ws-node .ws-chip .nm {
          font-size:10px;
          color:var(--text-2);
        }

        .ws-node .ws-worker-hint {
          font-size:10px;
          color:var(--faint);
        }
      `}</style>

      <Handle
        type="target"
        position={Position.Left}
        style={handleStyle}
      />

      <div className={`ws-step${selected ? ' sel' : ''}`}>
        <div className="ws-step-hd">
          <div className="ws-idx">{nodeData.index ?? ''}</div>
          <div className="ws-step-name">{nodeData.name ?? ''}</div>
          <div className={`ws-dyn ${nodeData.dynamic === true ? 'on' : 'off'}`}>
            {nodeData.dynamic === true ? '⟳ self-spawn' : 'fixed'}
          </div>
        </div>

        <div className="ws-step-bd">
          <div className="ws-mgr">
            <div
              className="ws-mgr-mono"
              style={{ background: nodeData.managerColor }}
            >
              {nodeData.managerMono ?? ''}
            </div>
            <span className="ws-mgr-tag">manager ·</span>
            <span className="ws-mgr-label">
              {nodeData.managerLabel ?? ''}
            </span>
            {nodeData.ask === true && (
              <span className="ws-ask">ask ✓</span>
            )}
          </div>

          {hasPersona && (
            <div className="ws-persona">{nodeData.persona}</div>
          )}

          <div className="ws-directive">
            <div className="lb">BEHAVIOR / DIRECTIVE</div>
            <div className="bd">{nodeData.behavior ?? '—'}</div>
          </div>

          <div className="ws-workers">
            {workers.length === 0 ? (
              <span className="ws-worker-hint">drop a CLI/role</span>
            ) : (
              workers.map((worker, index) => {
                const color = worker?.color || '#000000';

                return (
                  <div
                    className="ws-chip"
                    key={`${worker?.mono ?? ''}-${worker?.label ?? ''}-${index}`}
                    style={{
                      background: hexA(color, 0.14),
                      border: `1px solid ${hexA(color, 0.5)}`,
                    }}
                  >
                    <span
                      className="ws-chip-mono"
                      style={{ background: color }}
                    >
                      {worker?.mono ?? ''}
                    </span>
                    <span className="nm">{worker?.label ?? ''}</span>
                  </div>
                );
              })
            )}
          </div>
        </div>
      </div>

      <Handle
        type="source"
        position={Position.Right}
        style={handleStyle}
      />
    </div>
  );
}
