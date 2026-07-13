// Small controlled form primitives for the Studio inspector. Styled by studio.css.

export function Field({ label, mono, children }) {
  return (
    <label className="sd-field">
      <span className={"sd-flabel" + (mono ? " mono" : "")}>{label}</span>
      {children}
    </label>
  );
}

export function Text({ value, onChange, placeholder, mono }) {
  return (
    <input
      className={"sd-input" + (mono ? " mono" : "")}
      value={value ?? ""}
      placeholder={placeholder || ""}
      onChange={(e) => onChange(e.target.value)}
    />
  );
}

export function Area({ value, onChange, placeholder, rows = 3 }) {
  return (
    <textarea
      className="sd-input sd-area"
      rows={rows}
      value={value ?? ""}
      placeholder={placeholder || ""}
      onChange={(e) => onChange(e.target.value)}
    />
  );
}

export function Num({ value, onChange, min = 0, placeholder }) {
  return (
    <input
      type="number"
      className="sd-input"
      min={min}
      value={value ?? ""}
      placeholder={placeholder || ""}
      onChange={(e) => onChange(e.target.value === "" ? null : Math.max(min, parseInt(e.target.value, 10) || 0))}
    />
  );
}

export function Toggle({ checked, onChange, label }) {
  return (
    <button type="button" className={"sd-toggle" + (checked ? " on" : "")} onClick={() => onChange(!checked)}>
      <span className="sd-toggle-dot" />
      {label}
    </button>
  );
}

export function Select({ value, onChange, options, placeholder }) {
  return (
    <select className="sd-input" value={value ?? ""} onChange={(e) => onChange(e.target.value || null)}>
      <option value="">{placeholder || "—"}</option>
      {options.map((o) => (
        <option key={o.value} value={o.value}>
          {o.label}
        </option>
      ))}
    </select>
  );
}

// Multi-select of backend ids as toggleable chips (order = selection order).
export function BackendChips({ selected, all, onToggle }) {
  return (
    <div className="sd-chips">
      {all.map((b) => (
        <button
          key={b}
          type="button"
          className={"sd-bchip" + (selected.includes(b) ? " on" : "")}
          onClick={() => onToggle(b)}
        >
          {b}
        </button>
      ))}
    </div>
  );
}
