/// <reference types="vitest/config" />
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
  test: {
    coverage: {
      provider: "v8",
      reporter: ["text", "json-summary", "html"],
      include: ["src/**/*.ts"],
      // Excludes: test files themselves, the bootstrap entry, and
      // type-only declaration files (no runtime to measure).
      exclude: ["src/**/*.test.ts", "src/main.ts", "src/**/*.d.ts"],
      // Ratchet protocol mirrors the Rust side: floor at
      // floor(actual - 1). Baseline measured 2026-05-17 at 26.61%
      // lines (counting all src/**/*.ts, including never-imported
      // files — honest measurement, not "what tests happen to
      // import"). Floor is 25. Bump on every PR that raises actual
      // coverage. Never lower.
      thresholds: {
        lines: 25,
      },
    },
  },
});
