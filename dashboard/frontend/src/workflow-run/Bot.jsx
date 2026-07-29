// Status-driven mascot for the stage view, modeled on the agentpit app icon:
// a charcoal pod with a glowing mint core and spoke nodes. Purely decorative —
// every fact it conveys (status) is also on the card, so it can be skimmed past.

const FACES = {
  running: (
    <g className="wrb-face">
      <ellipse className="wrb-eye-o" cx="39.5" cy="46" rx="4.5" ry="6" />
      <ellipse className="wrb-eye-o" cx="60.5" cy="46" rx="4.5" ry="6" />
      <circle className="wrb-glint" cx="41" cy="44" r="1.6" />
      <circle className="wrb-glint" cx="62" cy="44" r="1.6" />
      <path d="M46.5 57 q3.5 2.5 7 0" />
    </g>
  ),
  done: (
    <g className="wrb-face">
      <path d="M35 47 q4.5 -6 9 0" />
      <path d="M56 47 q4.5 -6 9 0" />
      <path className="wrb-mouth-open" d="M44 54 q6 7 12 0 z" />
    </g>
  ),
  failed: (
    <g className="wrb-face">
      <line x1="35.5" y1="42" x2="43.5" y2="50" />
      <line x1="43.5" y1="42" x2="35.5" y2="50" />
      <line x1="56.5" y1="42" x2="64.5" y2="50" />
      <line x1="64.5" y1="42" x2="56.5" y2="50" />
      <path d="M44 59 q6 -5 12 0" />
    </g>
  ),
};

export default function Bot({ status = "running", isRoot = false }) {
  return (
    <svg className={`wrb wrb-${status}${isRoot ? " wrb-is-root" : ""}`} viewBox="0 0 100 108">
      <line className="wrb-ant" x1="50" y1="20" x2="50" y2="10" />
      <circle className="wrb-node" cx="50" cy="8" r="4" />
      <line className="wrb-spoke" x1="21" y1="34" x2="13" y2="29" />
      <circle className="wrb-node" cx="11" cy="28" r="3.5" />
      <line className="wrb-spoke" x1="79" y1="34" x2="87" y2="29" />
      <circle className="wrb-node" cx="89" cy="28" r="3.5" />
      {/* the manager wears the icon's full five-node spoke ring */}
      {isRoot ? (
        <>
          <line className="wrb-spoke" x1="18" y1="62" x2="9" y2="66" />
          <circle className="wrb-node" cx="7" cy="67" r="3.5" />
          <line className="wrb-spoke" x1="82" y1="62" x2="91" y2="66" />
          <circle className="wrb-node" cx="93" cy="67" r="3.5" />
        </>
      ) : null}
      <g className="wrb-arm wrb-arm-l">
        <rect x="12" y="52" width="9" height="20" rx="4.5" />
      </g>
      <g className="wrb-arm wrb-arm-r">
        <rect x="79" y="52" width="9" height="20" rx="4.5" />
      </g>
      <rect className="wrb-leg" x="33" y="84" width="11" height="13" rx="5" />
      <rect className="wrb-leg" x="56" y="84" width="11" height="13" rx="5" />
      <rect className="wrb-body" x="19" y="19" width="62" height="68" rx="26" />
      <circle className="wrb-core" cx="50" cy="71" r="7.5" />
      <circle className="wrb-core-dot" cx="59" cy="76" r="2" />
      <circle className="wrb-blush" cx="29" cy="55" r="4.5" />
      <circle className="wrb-blush" cx="71" cy="55" r="4.5" />
      {FACES[status] || FACES.running}
    </svg>
  );
}
