// Small controlled form primitives for the Studio inspector. Styled by studio.css.
import { useId } from "react";
import { t as tr } from "./i18n.js";

export function Field({ label, mono, children }) {
  return (
    <label className="sd-field">
      <span className={"sd-flabel" + (mono ? " mono" : "")}>{label}</span>
      {children}
    </label>
  );
}

export function Text({ value, onChange, placeholder, mono, options = [] }) {
  const generatedListId = useId();
  const listId = options.length ? `sd-options-${generatedListId.replaceAll(":", "")}` : undefined;
  return (
    <>
      <input
        className={"sd-input" + (mono ? " mono" : "")}
        value={value ?? ""}
        placeholder={placeholder || ""}
        list={listId}
        onChange={(e) => onChange(e.target.value)}
      />
      {listId ? (
        <datalist id={listId}>
          {options.map((option) => (
            <option
              key={option.value}
              value={option.value}
              label={option.label && option.label !== option.value ? option.label : undefined}
            />
          ))}
        </datalist>
      ) : null}
    </>
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

// Tri-state override: inherit (null) / on (true) / off (false).
export function TriState({ value, onChange }) {
  const v = value === true ? "on" : value === false ? "off" : "";
  return (
    <select
      className="sd-input"
      value={v}
      onChange={(e) => onChange(e.target.value === "on" ? true : e.target.value === "off" ? false : null)}
    >
      <option value="">{tr("inherit")}</option>
      <option value="on">{tr("on")}</option>
      <option value="off">{tr("off")}</option>
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
