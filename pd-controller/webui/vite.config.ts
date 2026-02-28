import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import path from "node:path";

const controllerTarget = process.env.VITE_CONTROLLER_URL ?? "http://127.0.0.1:9100";

export default defineConfig(({ command }) => ({
  // Bundle is served by pd-controller at /ui and /ui/*.
  base: command === "build" ? "/ui/" : "/",
  plugins: [react()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "src")
    }
  },
  server: {
    port: 5173,
    proxy: {
      "/v1": {
        target: controllerTarget,
        ws: true
      },
      "/healthz": controllerTarget,
      "/metrics": controllerTarget
    }
  }
}));
