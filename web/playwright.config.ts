import { defineSmokeConfig } from './e2e/shared/config';

// Beam web-client adoption of the shared browser E2E smoke contract.
// `defineSmokeConfig` supplies shared browser, timeout, trace, and
// screenshot defaults; everything below is app-local.
//
// The shared module lives at `web/e2e/shared/` (vendored — copied, not
// symlinked). Refresh by re-copying from the shared smoke-test foundation
// at vendor-bump time.
const config = defineSmokeConfig({
  baseURL: 'http://127.0.0.1:4173',
  webServer: {
    // Build the production bundle and serve it with Vite's preview server.
    // We smoke the real built artifact, not the dev server.
    command: 'npm run build && npm run preview -- --host 127.0.0.1 --port 4173',
    url: 'http://127.0.0.1:4173',
    reuseExistingServer: !process.env.CI,
    timeout: 120_000,
  },
});

// Constrain Playwright to `.spec.ts` so it does not collect the vendored
// `e2e/shared/tests/*.test.ts` vitest suites. `testIgnore` belt-and-braces
// the same intent in case a future spec lands under `shared/`.
config.testMatch = '**/*.spec.ts';
config.testIgnore = '**/shared/**';

export default config;
