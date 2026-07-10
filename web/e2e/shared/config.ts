/**
 * Shared Playwright config for browser E2E smoke suites.
 *
 * Encodes shared defaults (browsers, timeouts, screenshots, traces) so
 * smoke output stays homogeneous. The consuming spec supplies either a
 * `preview` block (the standard vite build+preview webServer — the port
 * is resolved HERE, see below) or a fully app-local `baseURL`/`webServer`
 * pair; everything else has a defensible default.
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

// ---------------------------------------------------------------------------
// Per-run preview-port derivation.
//
// Shared self-hosted CI runners can execute several browser-smoke jobs
// concurrently on one host. A single hard-pinned 4173 collides: when 4173 is
// held by a sibling job, `vite preview` WITHOUT `--strictPort` silently
// drifts to the next free port while Playwright keeps polling the configured
// URL — so the suite talks to a FOREIGN app, then gets ERR_CONNECTION_REFUSED
// mid-suite when that sibling job tears down. Two coupled fixes:
//
//   1. derive the port per CI run from GITHUB_RUN_ID so concurrent jobs don't
//      contend for the same one (local dev keeps the conventional 4173). A
//      curated high band (10000-10999) is used because Chromium refuses to
//      connect to its "unsafe ports" (net::ERR_UNSAFE_PORT) before a request
//      is ever made;
//   2. `--strictPort` (threaded by {@link defineSmokeConfig}'s `preview`
//      block) so a residual collision is a FAST, LOUD bind failure instead of
//      a silent wrong-app fail-open.
//
// Chromium's restricted-ports list contains 10080 (the `amanda` service) — a
// run whose `GITHUB_RUN_ID % 1000 == 80` would derive 10080 and every
// `page.goto` would fail with net::ERR_UNSAFE_PORT before connecting. 10080
// is the ONLY Chromium-unsafe port in 10000-10999, so it is deterministically
// remapped to the adjacent safe slot.
//
// Rejected alternatives: ephemeral `:0` (fights Playwright's up-front
// `webServer.url` contract), runner-name-index schemes (a per-repo slot
// index collides deterministically across repos), and content-hash ports
// (birthday collisions with no mitigation story beyond run-id modulo).
// ---------------------------------------------------------------------------

/** Conventional local-dev vite preview port (kept for `vite preview` parity). */
export const LOCAL_PREVIEW_PORT = 4173;

/** Conventional local-dev gateway-CSP smoke port (one above the preview). */
export const LOCAL_GATEWAY_CSP_PORT = 4174;

/** Base of the per-run CI preview-port band (10000-10999). */
export const PREVIEW_PORT_BASE = 10_000;

/**
 * Base of the per-run CI gateway-CSP port band (11000-11999). Deliberately a
 * DISJOINT band, not `preview + 1`: adjacent pairing would let run A's
 * gateway port collide with run B's preview port. The 11000-11999 band
 * contains no Chromium-unsafe ports.
 */
export const GATEWAY_CSP_PORT_BASE = 11_000;

/** Width of each per-run port band. */
export const PORT_BAND_SIZE = 1_000;

/**
 * Chromium-unsafe ports inside the preview band. 10080 (`amanda`) is the only
 * one in 10000-10999; its +1 neighbour is safe, so the remap can never land
 * on another unsafe port.
 */
export const CHROMIUM_UNSAFE_PORTS: ReadonlySet<number> = new Set([10_080]);

/**
 * Map a GitHub Actions run id onto the Chromium-safe preview band.
 * An unparsable run id falls back to {@link LOCAL_PREVIEW_PORT}.
 */
export function derivePreviewPort(runId: string): number {
  // `Number('')` is 0, not NaN — treat blank input as unparsable explicitly.
  const parsed = runId.trim() === '' ? Number.NaN : Number(runId);
  if (!Number.isFinite(parsed)) {
    return LOCAL_PREVIEW_PORT;
  }
  const port = PREVIEW_PORT_BASE + (Math.abs(Math.trunc(parsed)) % PORT_BAND_SIZE);
  return CHROMIUM_UNSAFE_PORTS.has(port) ? port + 1 : port;
}

/**
 * The vite-preview port for this process: per-run in CI (GITHUB_RUN_ID set),
 * the conventional 4173 for local dev. Deterministic within one CI job, so
 * every caller in the same process (config factory, gateway-csp pairing,
 * spec files) resolves the SAME port.
 */
export function resolvePreviewPort(env: Record<string, string | undefined> = process.env): number {
  const runId = env.GITHUB_RUN_ID;
  return runId ? derivePreviewPort(runId) : LOCAL_PREVIEW_PORT;
}

/**
 * The gateway-CSP smoke port paired with {@link resolvePreviewPort}: the
 * preview slot shifted into the disjoint 11000-11999 band in CI, the
 * conventional 4174 for local dev. The gateway-CSP launcher (where a suite
 * vendors one) derives the SAME pair from GITHUB_RUN_ID, so config and
 * launcher cannot desync.
 */
export function resolveGatewayCspPort(
  env: Record<string, string | undefined> = process.env
): number {
  const preview = resolvePreviewPort(env);
  if (preview === LOCAL_PREVIEW_PORT) {
    return LOCAL_GATEWAY_CSP_PORT;
  }
  return preview - PREVIEW_PORT_BASE + GATEWAY_CSP_PORT_BASE;
}

/**
 * Standard vite build+preview webServer, with the port resolved by the
 * shared module (NOT hand-written per suite — hand-copied port math is how
 * consumers end up pinning one colliding port).
 *
 * {@link defineSmokeConfig} threads the ONE resolved port into
 * `use.baseURL`, `webServer.command` and `webServer.url`, so they cannot
 * desync, and always appends `--strictPort` — the load-bearing flag that
 * turns a port collision into a loud bind failure instead of a silent
 * wrong-app serve.
 */
export interface SmokePreviewOverrides {
  /** Command that builds the bundle before preview. Default `npm run build`. */
  buildCommand?: string;
  /**
   * Extra args appended to `npm run preview --` BEFORE the shared
   * `--host/--port/--strictPort` flags (e.g. `['--outDir', 'dist']`).
   */
  previewArgs?: string[];
  /** Explicit port override (tests only). Default {@link resolvePreviewPort}. */
  port?: number;
  /** webServer startup timeout in ms. Defaults to 180_000. */
  timeout?: number;
  /** Extra env passed to the webServer process. */
  env?: Record<string, string>;
}

interface SmokeConfigBaseOverrides {
  /** Where the test files live, relative to the repo's playwright.config.ts. */
  testDir?: string;
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

/**
 * Manual server shape: the app supplies its own pre-built `baseURL` (and
 * optionally `webServer`). Kept for non-vite suites; vite suites should
 * use the `preview` shape so the port stays shared-module-owned.
 */
export interface SmokeManualServerOverrides extends SmokeConfigBaseOverrides {
  /** Base URL the browser navigates against. App-local. */
  baseURL: string;
  /** webServer block — Playwright starts this before tests run. */
  webServer?: PlaywrightTestConfig['webServer'];
  preview?: never;
}

/**
 * Preview server shape: the shared module owns port resolution and threads
 * it into baseURL + webServer.command + webServer.url atomically.
 */
export interface SmokePreviewServerOverrides extends SmokeConfigBaseOverrides {
  preview: SmokePreviewOverrides;
  /** Optional baseURL override; defaults to the resolved preview URL. */
  baseURL?: string;
  webServer?: never;
}

/**
 * Overrides accepted by {@link defineSmokeConfig}. We intentionally don't
 * accept arbitrary {@link PlaywrightTestConfig} keys — staying explicit
 * about the override surface keeps the shared contract narrow.
 */
export type SmokeConfigOverrides = SmokeManualServerOverrides | SmokePreviewServerOverrides;

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
 * // Standard vite consumer: the shared module resolves the per-run port
 * // and threads it into baseURL + webServer.command + webServer.url.
 * export default defineSmokeConfig({
 *   preview: {},
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

  let baseURL = overrides.baseURL;
  let webServer = overrides.webServer as PlaywrightTestConfig['webServer'] | undefined;

  if (overrides.preview !== undefined) {
    if (webServer !== undefined) {
      // Unreachable through the typed surface; guards untyped JS callers.
      throw new Error('defineSmokeConfig: pass either `preview` or `webServer`, not both');
    }
    const preview = overrides.preview;
    const port = preview.port ?? resolvePreviewPort();
    const url = `http://127.0.0.1:${port}`;
    const buildCommand = preview.buildCommand ?? 'npm run build';
    const previewArgs =
      preview.previewArgs !== undefined && preview.previewArgs.length > 0
        ? `${preview.previewArgs.join(' ')} `
        : '';
    // `127.0.0.1` (not `localhost`) avoids the IPv6/IPv4 readiness-probe hang
    // Playwright hits when the OS resolves `localhost` to `::1` while preview
    // listens on IPv4. `--strictPort` is the fail-loud half of the port scheme.
    const command =
      `${buildCommand} && npm run preview -- ${previewArgs}` +
      `--host 127.0.0.1 --port ${port} --strictPort`;
    webServer = {
      command,
      url,
      reuseExistingServer: !inCI,
      timeout: preview.timeout ?? 180_000,
      // Surface preview-server stdout/stderr on failure so a genuine startup
      // crash (build error, strictPort bind refusal) is distinguishable from
      // a slow start in the CI log.
      stdout: 'pipe',
      stderr: 'pipe',
      ...(preview.env !== undefined ? { env: preview.env } : {}),
    };
    baseURL = baseURL ?? url;
  }

  if (baseURL === undefined) {
    // Unreachable through the typed surface; guards untyped JS callers.
    throw new Error('defineSmokeConfig: `baseURL` is required when no `preview` block is given');
  }

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
      baseURL,
    },
    projects,
  };

  if (webServer !== undefined) {
    config.webServer = webServer;
  }

  return config;
}
