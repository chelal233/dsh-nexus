import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    // Browser-only development preview: same-origin proxy to an Agent
    // started outside Tauri (no CORS involvement). Production runs inside
    // the launcher and uses the Tauri command instead.
    proxy: {
      "/agent": {
        // Override via NEXUS_DEV_AGENT_URL when needed; hardcoded so the
        // vite config stays browser-typing friendly.
        target: (globalThis as Record<string, unknown> & { process?: { env?: Record<string, string | undefined> } }).process?.env?.NEXUS_DEV_AGENT_URL || "http://127.0.0.1:31777",
        changeOrigin: true,
        rewrite: (path) => path.replace(/^\/agent/, ""),
      },
    },
  },
});
