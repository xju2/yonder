import { defineConfig } from "vite";

// Tauri serves the built files itself; in dev it loads this server.
export default defineConfig({
  clearScreen: false,
  server: { port: 5173, strictPort: true },
  build: { target: "safari16", outDir: "dist", chunkSizeWarningLimit: 8000 },
});
