import { useEffect, useState } from "react";
import * as api from "./bridge.js";
import { inputsOf } from "./canvas.js";

const CWD_KEY = "agentpit.loops.cwd";

function lastCwd() {
  try {
    return localStorage.getItem(CWD_KEY) || "";
  } catch {
    return "";
  }
}

// Start a loop from a saved blueprint file: its inputs, the directory it works in, and a
// title. The file is frozen into the loop when it starts (later edits do not touch it).
export default function StartDialog({ t, entry, project, onClose, onStarted }) {
  const [doc, setDoc] = useState(null);
  const [values, setValues] = useState({});
  const [cwd, setCwd] = useState(project || lastCwd());
  const [title, setTitle] = useState("");
  const [startNow, setStartNow] = useState(true);
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState(null);
  const [diags, setDiags] = useState([]);
  // One op id per dialog: a retried click after a timeout never starts a second loop.
  const [opId] = useState(api.newOpId);

  useEffect(() => {
    api
      .getBlueprint(entry.scope, entry.name, project)
      .then((b) => {
        setDoc(b.doc);
        const init = {};
        for (const i of inputsOf(b.doc)) init[i.name] = String(i.default ?? "");
        setValues(init);
      })
      .catch((e) => setErr(e.message));
  }, [entry, project]);

  const inputs = doc ? inputsOf(doc) : [];
  const missing = inputs.filter((i) => i.required && !String(values[i.name] || "").trim());

  const submit = async () => {
    setBusy(true);
    setErr(null);
    setDiags([]);
    try {
      const inputsOut = {};
      for (const [k, v] of Object.entries(values)) if (String(v).length) inputsOut[k] = String(v);
      const started = await api.startLoop({
        blueprint: { source: "path", path: entry.path },
        inputs: inputsOut,
        cwd: cwd.trim(),
        title: title.trim() || null,
        start: startNow,
        op_id: opId,
      });
      try {
        localStorage.setItem(CWD_KEY, cwd.trim());
      } catch {
        // best-effort
      }
      onStarted(started.loop_id);
    } catch (e) {
      setErr(e.message);
      setDiags(e.details?.diagnostics || []);
      setBusy(false);
    }
  };

  return (
    <div className="lp-modal-bg" onClick={onClose}>
      <div className="lp-modal" onClick={(e) => e.stopPropagation()}>
        <div className="lp-modal-hd">
          {t("Start {name}", { name: entry.name })}
          <span className="lp-muted">{entry.rev}</span>
        </div>
        {inputs.map((i) => (
          <label key={i.name} className="lp-field">
            <span>
              {i.name}
              {i.required ? " *" : ""}
              {i.description ? <em className="lp-muted"> — {i.description}</em> : null}
            </span>
            <textarea
              className="lp-input"
              rows={i.name === "goal" ? 3 : 1}
              value={values[i.name] ?? ""}
              onChange={(e) => setValues((v) => ({ ...v, [i.name]: e.target.value }))}
            />
          </label>
        ))}
        <label className="lp-field">
          <span>{t("Working directory")} *</span>
          <input className="lp-input" placeholder="/path/to/repo" value={cwd} onChange={(e) => setCwd(e.target.value)} />
        </label>
        <label className="lp-field">
          <span>{t("Title (optional)")}</span>
          <input className="lp-input" value={title} onChange={(e) => setTitle(e.target.value)} />
        </label>
        <label className="lp-check">
          <input type="checkbox" checked={startNow} onChange={(e) => setStartNow(e.target.checked)} />
          {t("Start right away")}
        </label>
        {err ? <div className="lp-err">{err}</div> : null}
        {diags.length ? (
          <ul className="lp-diags">
            {diags.map((d, i) => (
              <li key={i} className={`lp-diag-${d.severity}`}>
                {d.path ? <code>{d.path}</code> : null} {d.message}
              </li>
            ))}
          </ul>
        ) : null}
        <div className="lp-modal-ft">
          <button className="lp-btn" onClick={onClose}>
            {t("Cancel")}
          </button>
          <button className="lp-btn pri" disabled={busy || !doc || missing.length > 0 || !cwd.trim()} onClick={submit}>
            {busy ? t("Starting…") : t("Start")}
          </button>
        </div>
      </div>
    </div>
  );
}
