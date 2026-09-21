import { defineConfig } from "vite";

export default defineConfig({
  base: process.env.VITE_BASE || "/",
  server: {
    port: 5173,
    strictPort: true,
    proxy: {
      "/api": "http://127.0.0.1:8080",
      "/health": "http://127.0.0.1:8080",
      "/ws": { target: "ws://127.0.0.1:8080", ws: true },
    },
  },
  preview: {
    port: 4173,
    strictPort: true,
  },
});
