import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// base: "./" → relative asset URLs. Tauri serves the embedded frontendDist from
// the app root over its asset protocol; relative paths resolve correctly there
// (an absolute "/assets/..." base also works in Tauri v2, but "./" is portable
// and keeps the `python3 -m http.server` preview harness working too).
export default defineConfig({
  plugins: [react()],
  base: "./",
  build: { outDir: "dist", emptyOutDir: true },
  server: { port: 5173, strictPort: true },
});
