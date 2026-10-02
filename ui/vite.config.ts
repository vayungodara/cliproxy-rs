import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";

export default defineConfig({
  base: "./",
  plugins: [svelte()],
  server: {
    port: 5173,
    proxy: {
      "/v8": "http://127.0.0.1:8317",
      "/healthz": "http://127.0.0.1:8317",
    },
  },
  build: { target: "es2022", sourcemap: false },
});
