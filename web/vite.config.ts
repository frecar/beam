/// <reference types="vitest/config" />
import { sentryVitePlugin } from '@sentry/vite-plugin';
import { visualizer } from 'rollup-plugin-visualizer';
import { defineConfig } from 'vite';

const sentryUploadEnabled = Boolean(
  process.env.SENTRY_AUTH_TOKEN && process.env.SENTRY_ORG && process.env.SENTRY_PROJECT
);

const releaseName =
  process.env.SENTRY_RELEASE ?? process.env.VITE_BEAM_RELEASE ?? process.env.npm_package_version;

export default defineConfig(({ mode }) => {
  const plugins = [];
  if (sentryUploadEnabled) {
    plugins.push(
      sentryVitePlugin({
        org: process.env.SENTRY_ORG,
        project: process.env.SENTRY_PROJECT,
        authToken: process.env.SENTRY_AUTH_TOKEN,
        url: process.env.SENTRY_URL,
        telemetry: false,
        release: releaseName ? { name: releaseName } : undefined,
        sourcemaps: {
          assets: './dist/assets/**',
        },
      })
    );
  }
  if (mode === 'analyze' || process.env.BUNDLE_ANALYZE === '1') {
    plugins.push(
      visualizer({
        filename: 'dist/bundle-stats.html',
        gzipSize: true,
        brotliSize: true,
        template: 'treemap',
      })
    );
  }

  return {
    plugins,
    build: {
      sourcemap: true,
    },
    server: {
      proxy: {
        '/api': {
          target: 'https://localhost:8444',
          secure: false,
          changeOrigin: true,
        },
        '/ws': {
          target: 'wss://localhost:8444',
          secure: false,
          ws: true,
        },
      },
    },
    test: {
      // Vitest owns `.test.ts`; Playwright owns `.spec.ts`. An explicit
      // include keeps vitest from ever trying to run the browser smoke
      // specs under e2e/ (which would fail — they need a real browser).
      // The vendored E2E helper unit tests DO run here so the helpers
      // stay covered without padding the src coverage ratchet.
      include: ['src/**/*.test.ts', 'e2e/shared/tests/**/*.test.ts'],
      coverage: {
        provider: 'v8',
        reporter: ['text', 'json-summary', 'html'],
        // Scope the ratchet to src/ only — the vendored e2e helpers are
        // covered by their own tests but excluded from the src %.
        include: ['src/**/*.ts'],
        // Excludes: test files themselves, the bootstrap entry, and
        // type-only declaration files (no runtime to measure).
        exclude: ['src/**/*.test.ts', 'src/main.ts', 'src/**/*.d.ts'],
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
  };
});
