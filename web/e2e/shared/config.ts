/**
 * Shared Playwright config for browser E2E smoke suites.
 *
 * Encodes shared defaults (browsers, timeouts, screenshots, traces) so
 * smoke output stays homogeneous. The consuming spec supplies its own
 * `baseURL` and `webServer` because those are app-local decisions;
 * everything else has a defensible default.
 *
 * Vendored from a shared smoke-test foundation — see web/e2e/README.md.
 */

import type {
  PlaywrightTestConfig,
  PlaywrightTestProject,
  ReporterDescription,
} from '@playwright/test';

/**
 * Browser projects that ship by default. Repos can opt out (e.g. drop
 * firefox/webkit if a workflow is Chromium-specific) by passing
 * `projects: [...]` to {@link defineSmokeConfig}.
 *
 * Defaults to chromium-only — keeps CI quick. Adoption phases that
 * genuinely need cross-browser parity should opt in explicitly.
 */
export const DEFAULT_BROWSER_PROJECTS: PlaywrightTestProject[] = [
  { name: 'chromium', use: { browserName: 'chromium' } },
];

/**
 * Multi-browser project preset. Use this when a suite needs Firefox +
 * WebKit parity (rare for an internal tool).
 */
export const ALL_BROWSER_PROJECTS: PlaywrightTestProject[] = [
  { name: 'chromium', use: { browserName: 'chromium' } },
  { name: 'firefox', use: { browserName: 'firefox' } },
  { name: 'webkit', use: { browserName: 'webkit' } },
];

/**
 * Overrides accepted by {@link defineSmokeConfig}. We intentionally don't
 * accept arbitrary {@link PlaywrightTestConfig} keys — staying explicit
 * about the override surface keeps the shared contract narrow.
 */
export interface SmokeConfigOverrides {
  /** Where the test files live, relative to the repo's playwright.config.ts. */
  testDir?: string;
  /** Base URL the browser navigates against. Repo-local. */
  baseURL: string;
  /** webServer block — Playwright starts this before tests run. */
  webServer?: PlaywrightTestConfig['webServer'];
  /** Browser projects. Defaults to {@link DEFAULT_BROWSER_PROJECTS}. */
  projects?: PlaywrightTestProject[];
  /** Per-test timeout in ms. Defaults to 30_000. */
  timeout?: number;
  /** expect() assertion timeout in ms. Defaults to 5_000. */
  expectTimeout?: number;
  /** Number of retries in CI. Defaults to 1; outside CI defaults to 0. */
  retries?: number;
  /** Number of parallel workers. Defaults to 1 (deterministic CI). */
  workers?: number;
  /** Extra reporters merged with the shared baseline. */
  extraReporters?: ReporterDescription[];
}

const SMOKE_USE_DEFAULTS = {
  // Screenshot the failing state — vendor-able CI uploads it.
  screenshot: 'only-on-failure' as const,
  // Keep a trace for post-mortem on failures, drop on pass.
  trace: 'retain-on-failure' as const,
  // Recording video bloats CI artifacts; keep off by default.
  video: 'off' as const,
  // Action timeout — narrower than the test timeout so flaky waits surface fast.
  actionTimeout: 10_000,
  // Navigation timeout — covers cold-start app reloads in CI.
  navigationTimeout: 30_000,
};

const isCI = (): boolean => process.env.CI !== undefined && process.env.CI !== '';

/**
 * Build a Playwright config with the shared smoke defaults. A consuming
 * `playwright.config.ts` calls this and merges its own overrides.
 *
 * @example
 * ```ts
 * import { defineSmokeConfig } from './shared/config';
 *
 * export default defineSmokeConfig({
 *   baseURL: 'http://127.0.0.1:4173',
 *   webServer: {
 *     command: 'npm run build && npm run preview',
 *     url: 'http://127.0.0.1:4173',
 *     timeout: 120_000,
 *   },
 * });
 * ```
 */
export function defineSmokeConfig(overrides: SmokeConfigOverrides): PlaywrightTestConfig {
  const inCI = isCI();
  const retries = overrides.retries ?? (inCI ? 1 : 0);
  const workers = overrides.workers ?? 1;
  const projects = overrides.projects ?? DEFAULT_BROWSER_PROJECTS;

  const reporters: ReporterDescription[] = [
    inCI ? ['github'] : ['list'],
    ['html', { open: 'never', outputFolder: 'playwright-report' }],
    ...(overrides.extraReporters ?? []),
  ];

  const config: PlaywrightTestConfig = {
    testDir: overrides.testDir ?? './e2e',
    timeout: overrides.timeout ?? 30_000,
    expect: {
      timeout: overrides.expectTimeout ?? 5_000,
    },
    fullyParallel: false,
    forbidOnly: inCI,
    retries,
    workers,
    reporter: reporters,
    use: {
      ...SMOKE_USE_DEFAULTS,
      baseURL: overrides.baseURL,
    },
    projects,
  };

  if (overrides.webServer !== undefined) {
    config.webServer = overrides.webServer;
  }

  return config;
}
