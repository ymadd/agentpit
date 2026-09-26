import { useEffect, useState } from "react";
import { edgeOnFor } from "./canvas.js";

// Design-mode inspector: the selected node's fields, the selected edge's condition, or the
// document's own settings. Only the fields listed here are edited; every other key of the
// node or document is left exactly as it was.

const FIELDS = {
  agent: [
    ["title", "text"],
    ["role", "text"],
    ["backend", "text"],
    ["model", "text"],
    ["effort", "text"],
    ["category", "text"],
    ["access", "select", ["write", "read"]],
    ["verdict", "bool"],
    ["retries", "number", 0, 3],
    ["timeout_secs", "number", 1, 86400],
    ["task", "area"],
  ],
  manager: [
    ["title", "text"],
    ["role", "text"],
    ["backend", "text"],
    ["model", "text"],
    ["max_calls", "number", 1, 32],
    ["timeout_secs", "number", 1, 86400],
    ["task", "area"],
  ],
  check: [
    ["title", "text"],
    ["command", "mono"],
    ["cwd", "text"],
    ["timeout_secs", "number", 1, 86400],
    ["retries", "number", 0, 3],
  ],
  gate: [
    ["title", "text"],
    ["prompt", "area"],
    ["timeout_secs", "number", 1, 604800],
    ["on_timeout", "text"],
  ],
  repeat: [
    ["title", "text"],
    ["max_iterations", "number", 1, 20],
    ["feedback", "select", ["last", "all", "none"]],
  ],
};

// A text box that edits a local copy and commits on blur (or Enter for one-liners), so
// each keystroke does not become a document revision.
function Field({ label, kind, value, onCommit, options, min, max }) {
  const [v, setV] = useState(value ?? "");
  useEffect(() => setV(value ?? ""), [value]);
  const commit = (next) => {
    if (kind === "number") {
      if (next === "" || next == null) return onCommit(undefined);
      const n = Math.round(Number(next));
      if (!Number.isFinite(n)) return;
      return onCommit(Math.min(max ?? n, Math.max(min ?? n, n)));
    }
    return onCommit(next);
  };
  if (kind === "bool") {
    return (
      <label className="lp-check">
        <input type="checkbox" checked={!!value} onChange={(e) => onCommit(e.target.checked || undefined)} />
        {label}
      </label>
    );
  }
  if (kind === "select") {
    return (
      <label className="lp-field">
        <span>{label}</span>
        <select className="lp-input" value={value ?? options[0]} onChange={(e) => onCommit(e.target.value)}>
          {options.map((o) => (
            <option key={o} value={o}>
              {o}
            </option>
          ))}
        </select>
      </label>
    );
  }
  const area = kind === "area";
  const Tag = area ? "textarea" : "input";
  return (
    <label className="lp-field">
      <span>{label}</span>
      <Tag
        className={`lp-input ${kind === "mono" || area ? "mono" : ""}`}
        rows={area ? 8 : undefined}
        type={kind === "number" ? "number" : "text"}
        min={min}
        max={max}
        value={v}
        onChange={(e) => setV(e.target.value)}
        onBlur={() => commit(v)}
        onKeyDown={(e) => {
          if (!area && e.key === "Enter") commit(v);
        }}
      />
    </label>
  );
}

function GateOptions({ t, options, onCommit }) {
  const list = options && options.length ? options : [];
  const set = (i, patch) => onCommit(list.map((o, j) => (j === i ? { ...o, ...patch } : o)));
  return (
    <div className="lp-field">
      <span>{t("options (empty = approve / reject)")}</span>
      {list.map((o, i) => (
        <div key={i} className="lp-opt-row">
          <input className="lp-input" value={o.id} placeholder="id" onChange={(e) => set(i, { id: e.target.value })} />
          <input
            className="lp-input"
            value={o.label || ""}
            placeholder="label"
            onChange={(e) => set(i, { label: e.target.value || undefined })}
          />
          <select className="lp-input" value={o.outcome || "ok"} onChange={(e) => set(i, { outcome: e.target.value })}>
            <option value="ok">ok</option>
            <option value="fail">fail</option>
          </select>
          <button className="lp-btn ghost" onClick={() => onCommit(list.filter((_, j) => j !== i))}>
            ✕
          </button>
        </div>
      ))}
      <button className="lp-btn" onClick={() => onCommit([...list, { id: `opt${list.length + 1}`, outcome: "ok" }])}>
        {t("＋ option")}
      </button>
    </div>
  );
}

export function NodeInspector({ t, node, diags, onChange, onDelete, readOnly }) {
  const fields = FIELDS[node.kind] || [["title", "text"]];
  return (
    <div className="lp-insp">
      <div className="lp-insp-hd">
        <span className="lp-node-ic">{node.kind}</span>
        <code>{node.id}</code>
        {node.parent ? <span className="lp-muted">in {node.parent}</span> : null}
        {!readOnly ? (
          <button className="lp-btn ghost" onClick={onDelete} title={t("Delete")}>
            ✕
          </button>
        ) : null}
      </div>
      {diags.map((d, i) => (
        <div key={i} className={`lp-diag lp-diag-${d.severity}`}>
          {d.message}
        </div>
      ))}
      <fieldset disabled={readOnly} className="lp-fields">
        {fields.map(([key, kind, a, b]) => (
          <Field
            key={key}
            label={key}
            kind={kind}
            value={node[key]}
            options={Array.isArray(a) ? a : undefined}
            min={typeof a === "number" ? a : undefined}
            max={typeof b === "number" ? b : undefined}
            onCommit={(v) => onChange({ [key]: v })}
          />
        ))}
        {node.kind === "gate" ? <GateOptions t={t} options={node.options} onCommit={(v) => onChange({ options: v.length ? v : undefined })} /> : null}
      </fieldset>
    </div>
  );
}

export function EdgeInspector({ t, edge, sourceKind, onChange, onDelete, readOnly }) {
  return (
    <div className="lp-insp">
      <div className="lp-insp-hd">
        <code>
          {edge.from} → {edge.to}
        </code>
        {!readOnly ? (
          <button className="lp-btn ghost" onClick={onDelete} title={t("Delete")}>
            ✕
          </button>
        ) : null}
      </div>
      <label className="lp-field">
        <span>{t("follow when the source ends")}</span>
        <select
          className="lp-input"
          disabled={readOnly}
          value={edge.on || "ok"}
          onChange={(e) => onChange(e.target.value)}
        >
          {edgeOnFor(sourceKind).map((o) => (
            <option key={o} value={o}>
              {o}
            </option>
          ))}
        </select>
      </label>
    </div>
  );
}

export function DocInspector({ t, doc, diags, onChange, readOnly }) {
  const budget = doc.budget || {};
  const policy = doc.policy || {};
  const setIn = (key, patch) => {
    const cur = { ...(doc[key] || {}) };
    for (const [k, v] of Object.entries(patch)) {
      if (v === undefined) delete cur[k];
      else cur[k] = v;
    }
    onChange({ [key]: Object.keys(cur).length ? cur : undefined });
  };
  const [inputs, setInputs] = useState(JSON.stringify(doc.inputs || {}, null, 2));
  const [inputsErr, setInputsErr] = useState(null);
  useEffect(() => setInputs(JSON.stringify(doc.inputs || {}, null, 2)), [doc.inputs]);
  return (
    <div className="lp-insp">
      <div className="lp-insp-hd">
        <code>{doc.name}</code>
        <span className="lp-muted">{t("blueprint")}</span>
      </div>
      {diags.map((d, i) => (
        <div key={i} className={`lp-diag lp-diag-${d.severity}`}>
          {d.path ? <code>{d.path}</code> : null} {d.message}
        </div>
      ))}
      <fieldset disabled={readOnly} className="lp-fields">
        <Field label="title" kind="text" value={doc.title} onCommit={(v) => onChange({ title: v || undefined })} />
        <Field
          label="description"
          kind="area"
          value={doc.description}
          onCommit={(v) => onChange({ description: v || undefined })}
        />
        <Field
          label="budget.max_steps"
          kind="number"
          min={1}
          max={500}
          value={budget.max_steps}
          onCommit={(v) => setIn("budget", { max_steps: v })}
        />
        <Field
          label="budget.max_active_secs"
          kind="number"
          min={60}
          max={604800}
          value={budget.max_active_secs}
          onCommit={(v) => setIn("budget", { max_active_secs: v })}
        />
        <Field
          label="budget.max_parallel"
          kind="number"
          min={1}
          max={8}
          value={budget.max_parallel}
          onCommit={(v) => setIn("budget", { max_parallel: v })}
        />
        <Field
          label="policy.on_error"
          kind="select"
          options={["gate", "fail"]}
          value={policy.on_error}
          onCommit={(v) => setIn("policy", { on_error: v })}
        />
        <Field
          label="policy.on_budget"
          kind="select"
          options={["gate", "fail"]}
          value={policy.on_budget}
          onCommit={(v) => setIn("policy", { on_budget: v })}
        />
        <label className="lp-field">
          <span>inputs (JSON)</span>
          <textarea
            className="lp-input mono"
            rows={6}
            value={inputs}
            onChange={(e) => setInputs(e.target.value)}
            onBlur={() => {
              try {
                const v = JSON.parse(inputs || "{}");
                setInputsErr(null);
                onChange({ inputs: v });
              } catch (e) {
                setInputsErr(String(e.message || e));
              }
            }}
          />
        </label>
        {inputsErr ? <div className="lp-err">{inputsErr}</div> : null}
      </fieldset>
    </div>
  );
}
