import { createRoot } from "react-dom/client";
import SettingsApp from "./settings/SettingsApp.jsx";
import { startAutoUpdater } from "./settings/app-update.js";
import WorkflowRunApp from "./workflow-run/WorkflowRunApp.jsx";

// Strangler bridge (Phase 2). The legacy vanilla dashboard (public/app.js) owns
// the page — statusbar, cockpit, swarm, CLI rail — and renders first. React
// owns the complete settings surface: app.js hands it #settings by calling the
// legacy-named window.__agentpitMountStudio(container) bridge. Workflow Studio
// now lives as one preserved section inside that shell.
let root = null;

window.__agentpitMountStudio = (container) => {
  if (!container) return;
  if (!root) root = createRoot(container);
  root.render(<SettingsApp />);
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

// Desktop is the release owner. Check the paired release after startup and, when
// enabled in Settings, let the bundled CLI install it in the background.
startAutoUpdater();
