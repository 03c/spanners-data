import { defineConfig } from "vite";

export default defineConfig({
  // Tauri expects a fixed port and must not fall back to another one.
  server: { port: 1420, strictPort: true },
  clearScreen: false,
  build: { target: "chrome105", minify: "esbuild" },
});
