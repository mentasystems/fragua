import { defineConfig } from "vite";

// Tauri 2 sets TAURI_DEV_HOST when iOS/Android dev is in play; on desktop
// the default localhost is fine. We pin a known port so src-tauri can
// reach the dev server.
export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: process.env.TAURI_DEV_HOST ?? "localhost",
  },
  build: {
    target: "es2022",
    minify: "esbuild",
    sourcemap: true,
  },
});
