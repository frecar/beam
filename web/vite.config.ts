import { defineConfig } from "vite";

export default defineConfig({
  server: {
    proxy: {
      "/api": {
        target: "https://localhost:8444",
        secure: false,
        changeOrigin: true,
      },
      "/ws": {
        target: "wss://localhost:8444",
        secure: false,
        ws: true,
      },
    },
  },
});
