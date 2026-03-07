import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

export default defineConfig({
  plugins: [react(), tailwindcss()],
  server: {
    port: 5173,
    proxy: {
      "/health": "http://127.0.0.1:8765",
      "/chunks": "http://127.0.0.1:8765",
      "/explore": "http://127.0.0.1:8765",
      "/notes": "http://127.0.0.1:8765",
    },
  },
});
