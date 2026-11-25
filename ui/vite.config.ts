import { defineConfig } from "vite";

export default defineConfig({
  clearScreen: false,
  server: { port: 1430, strictPort: true },
  build: { target: "safari15", assetsInlineLimit: 0 },
});
