import { createRoot } from "react-dom/client";
import StudioApp from "./studio/StudioApp.jsx";
import WorkflowRunApp from "./workflow-run/WorkflowRunApp.jsx";

// Strangler bridge (Phase 2). The legacy vanilla dashboard (public/app.js) owns
// the page — statusbar, cockpit, swarm, CLI rail — and renders first. React
// owns only the Workflow Studio: app.js hands it the #settings surface by
// calling window.__agentpitMountStudio(container). Dormant until called, so
// this build stays behavior-identical to the old dashboard until Phase 2b wires
// the call site.
let root = null;

window.__agentpitMountStudio = (container) => {
  if (!container) return;
  if (!root) root = createRoot(container);
  root.render(<StudioApp />);
};

window.__agentpitUnmountStudio = () => {
  if (root) {
    root.unmount();
    root = null;
  }
};

// Live "Workflow Run" stage view — a React island that owns its own container
// (a small launcher + a full React Flow overlay), so it never touches the
// legacy dashboard DOM. Polls get_snapshot; renders the manager → dispatched
// sub-run tree with live status.
const wrEl = document.createElement("div");
wrEl.id = "agentpit-workflow-run";
document.body.appendChild(wrEl);
createRoot(wrEl).render(<WorkflowRunApp />);
