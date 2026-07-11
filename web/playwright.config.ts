import { defineSmokeConfig } from './e2e/shared/config';

// Beam web-client adoption of the shared browser E2E smoke contract.
// `defineSmokeConfig` supplies shared browser, timeout, trace, and
// screenshot defaults; everything below is app-local.
//
// The shared module lives at `web/e2e/shared/` (vendored — copied, not
// symlinked). Refresh by re-copying from the shared smoke-test foundation
// at vendor-bump time.
//
// The preview port + `--strictPort` are owned by the shared module: it
// resolves a per-CI-run port from GITHUB_RUN_ID (4173 locally) and threads
// the one resolved value into `use.baseURL` + `webServer.command` +
// `webServer.url`, so a port collision on a shared runner is a loud bind
// failure — never a silent wrong-app serve. Do not hand-write ports here.
const config = defineSmokeConfig({
  preview: {
    // Build the production bundle and serve it with Vite's preview server.
    // We smoke the real built artifact, not the dev server.
    timeout: 120_000,
  },
});

// Constrain Playwright to `.spec.ts` so it does not collect the vendored
// `e2e/shared/tests/*.test.ts` vitest suites. `testIgnore` belt-and-braces
// the same intent in case a future spec lands under `shared/`.
config.testMatch = '**/*.spec.ts';
config.testIgnore = '**/shared/**';

export default config;
