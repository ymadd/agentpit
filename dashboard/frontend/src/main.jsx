import { createRoot } from "react-dom/client";
import StudioCanvas from "./App.jsx";

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
  root.render(<StudioCanvas />);
};

window.__agentpitUnmountStudio = () => {
  if (root) {
    root.unmount();
    root = null;
  }
};
