/**
 * Reusable Playwright assertion helpers for browser smoke suites.
 *
 * Each helper encodes a shared convention for what "healthy" / "logged-in"
 * looks like, so smoke specs don't reinvent the checks file-by-file.
 * Implementations stay thin — page-specific navigation belongs in the
 * spec files that call these helpers.
 *
 * Vendored from a shared smoke-test foundation — see web/e2e/README.md.
 */

import type { APIRequestContext, Page, Response } from '@playwright/test';
import { expect } from '@playwright/test';

// ---------------------------------------------------------------------------
// healthcheck200
// ---------------------------------------------------------------------------

export interface HealthcheckOptions {
  /** Path on the service to probe. Defaults to `/health`. */
  path?: string;
  /**
   * JSON keys that must be present in the response body. Defaults to
   * `['status']`. Pass `[]` to skip the body-shape check entirely.
   */
  expectedKeys?: string[];
  /** Optional exact value match for one of the expected keys. */
  expectedValues?: Record<string, unknown>;
}

/**
 * Asserts a service /health endpoint returns HTTP 200 with a JSON body
 * containing the expected keys.
 *
 * Convention: a web UI service exposes `/health` returning a JSON object
 * with at least a `status` key — `{ status: 'ok', timestamp, ... }`.
 *
 * @example
 * ```ts
 * await healthcheck200(request, 'http://127.0.0.1:8222');
 * await healthcheck200(request, baseURL, {
 *   path: '/api/health',
 *   expectedKeys: ['status', 'version'],
 *   expectedValues: { status: 'ok' },
 * });
 * ```
 */
export async function healthcheck200(
  request: APIRequestContext,
  baseURL: string,
  options: HealthcheckOptions = {}
): Promise<unknown> {
  const path = options.path ?? '/health';
  const expectedKeys = options.expectedKeys ?? ['status'];
  const url = `${trimTrailingSlash(baseURL)}${path}`;

  const response = await request.get(url);
  expect(response.status(), `${url} did not return 200`).toBe(200);

  if (expectedKeys.length === 0 && options.expectedValues === undefined) {
    return undefined;
  }

  let body: unknown;
  try {
    body = await response.json();
  } catch (err) {
    throw new Error(`${url} returned 200 but body did not parse as JSON: ${stringifyError(err)}`);
  }

  assertIsObject(body, url);

  for (const key of expectedKeys) {
    if (!(key in body)) {
      throw new Error(
        `${url} response JSON missing expected key '${key}'. Body keys: ${Object.keys(body).join(', ')}`
      );
    }
  }

  if (options.expectedValues !== undefined) {
    for (const [key, expectedValue] of Object.entries(options.expectedValues)) {
      if (body[key] !== expectedValue) {
        throw new Error(
          `${url} response key '${key}' = ${JSON.stringify(body[key])}, expected ${JSON.stringify(expectedValue)}`
        );
      }
    }
  }

  return body;
}

// ---------------------------------------------------------------------------
// loginPasses
// ---------------------------------------------------------------------------

export interface LoginAssertionOptions {
  /**
   * Path that indicates the user is STILL on the login page. The assertion
   * fails if the post-login URL still contains this path.
   *
   * Defaults to `'/login'` — matches Authentik's `/if/flow/.../login/`,
   * Django's `/local-login/`, and most ad-hoc login routes.
   */
  loginPathFragment?: string;
  /**
   * Optional locator (CSS selector or text content) that should be visible
   * after a successful login — typically a navbar element scoped to the
   * authenticated UI. If omitted, only the URL check is performed.
   */
  postLoginSelector?: string;
  /** Optional text the post-login selector should contain. */
  postLoginText?: string;
}

/**
 * Asserts that after a login attempt:
 *
 *  1. The current URL no longer contains the login-page fragment.
 *  2. (Optional) A known post-login locator is visible.
 *
 * This is intentionally NOT a "performs the login" helper — each repo's
 * auth flow is different (Authentik OIDC, Django local-login, etc.) and
 * encoding the click-path here would make the package brittle. Call the
 * repo's own sign-in helper, then call this to verify it worked.
 *
 * @example
 * ```ts
 * await signInViaAuthentik(page, operator);
 * await loginPasses(page, {
 *   loginPathFragment: '/if/flow',
 *   postLoginSelector: 'role=heading[name="Dashboard"]',
 * });
 * ```
 */
export async function loginPasses(page: Page, options: LoginAssertionOptions = {}): Promise<void> {
  const loginPathFragment = options.loginPathFragment ?? '/login';
  const currentUrl = page.url();

  if (currentUrl.includes(loginPathFragment)) {
    throw new Error(
      `loginPasses: URL still contains login fragment '${loginPathFragment}': ${currentUrl}`
    );
  }

  if (options.postLoginSelector !== undefined) {
    const locator = page.locator(options.postLoginSelector);
    if (options.postLoginText !== undefined) {
      await expect(locator).toContainText(options.postLoginText);
    } else {
      await expect(locator).toBeVisible();
    }
  }
}

// ---------------------------------------------------------------------------
// noConsoleErrors
// ---------------------------------------------------------------------------

export interface ConsoleErrorAssertionOptions {
  /**
   * Patterns to allow (string contains or RegExp test). Useful for
   * known-noisy third-party scripts (e.g. Sentry's "Failed to fetch" on
   * sample submission when the SDK is offline-tolerant).
   */
  allowlist?: ReadonlyArray<string | RegExp>;
  /**
   * Console levels to treat as failures. Defaults to `['error']`. Set
   * `['error', 'warning']` for stricter pages.
   */
  levels?: ReadonlyArray<'error' | 'warning'>;
}

export interface ConsoleRecorder {
  /** Stop recording and return collected console messages (filtered to the configured levels). */
  collect: () => { type: string; text: string; location: string }[];
  /**
   * Assert that no console error was emitted during the recording window.
   * Throws with an aggregated message listing every offending entry.
   */
  assertEmpty: () => void;
  /** Stop listening — call manually if not using `assertEmpty`. */
  detach: () => void;
}

/**
 * Starts recording browser console output, returning a {@link ConsoleRecorder}
 * the test calls after navigation to assert no errors were emitted.
 *
 * @example
 * ```ts
 * const console_ = recordConsoleErrors(page, { allowlist: [/Sentry/] });
 * await page.goto('/');
 * console_.assertEmpty();
 * ```
 */
export function recordConsoleErrors(
  page: Page,
  options: ConsoleErrorAssertionOptions = {}
): ConsoleRecorder {
  const levels = options.levels ?? ['error'];
  const allowlist = options.allowlist ?? [];
  const collected: { type: string; text: string; location: string }[] = [];

  const listener = (msg: import('@playwright/test').ConsoleMessage): void => {
    if (!(levels as readonly string[]).includes(msg.type())) {
      return;
    }
    const text = msg.text();
    if (matchesAllowlist(text, allowlist)) {
      return;
    }
    const loc = msg.location();
    collected.push({
      type: msg.type(),
      text,
      location: `${loc.url}:${loc.lineNumber}:${loc.columnNumber}`,
    });
  };

  page.on('console', listener);

  return {
    collect: () => [...collected],
    detach: () => {
      page.off('console', listener);
    },
    assertEmpty: () => {
      page.off('console', listener);
      if (collected.length === 0) {
        return;
      }
      const lines = collected.map((entry) => `  [${entry.type}] ${entry.text} @ ${entry.location}`);
      throw new Error(
        `noConsoleErrors: ${collected.length} console error(s) emitted:\n${lines.join('\n')}`
      );
    },
  };
}

// ---------------------------------------------------------------------------
// noNetworkErrors
// ---------------------------------------------------------------------------

export interface NetworkErrorAssertionOptions {
  /**
   * URL substrings or RegExps to ignore. Defaults to none — strict by
   * default. Use this for known-5xx endpoints (e.g. Sentry sample submission
   * that legitimately 503s when offline).
   */
  allowlist?: ReadonlyArray<string | RegExp>;
  /**
   * Only check requests whose URL starts with the given origin. Pass
   * `baseURL` to scope to same-origin only. Defaults to no scoping.
   */
  scopeToOrigin?: string;
  /**
   * Status threshold treated as a failure. Defaults to 500 — any 5xx fails.
   * Set 400 for stricter checks that also fail on 4xx.
   */
  failOnStatusGte?: number;
}

export interface NetworkRecorder {
  /** Return the list of failing responses captured so far. */
  collect: () => { url: string; status: number; method: string }[];
  /** Stop listening — call manually if not using `assertEmpty`. */
  detach: () => void;
  /**
   * Assert that no failing response was captured during the recording
   * window. Throws with an aggregated message.
   */
  assertEmpty: () => void;
}

/**
 * Starts recording browser network responses. The returned
 * {@link NetworkRecorder} surfaces same-origin (or scoped) responses whose
 * status is at or above the failure threshold (5xx by default).
 *
 * @example
 * ```ts
 * const net = recordNetworkErrors(page, { scopeToOrigin: baseURL });
 * await page.goto('/');
 * await page.getByRole('button', { name: 'Refresh' }).click();
 * net.assertEmpty();
 * ```
 */
export function recordNetworkErrors(
  page: Page,
  options: NetworkErrorAssertionOptions = {}
): NetworkRecorder {
  const threshold = options.failOnStatusGte ?? 500;
  const allowlist = options.allowlist ?? [];
  const scopePrefix = options.scopeToOrigin;
  const collected: { url: string; status: number; method: string }[] = [];

  const listener = (response: Response): void => {
    const url = response.url();
    if (scopePrefix !== undefined && !url.startsWith(scopePrefix)) {
      return;
    }
    if (matchesAllowlist(url, allowlist)) {
      return;
    }
    const status = response.status();
    if (status < threshold) {
      return;
    }
    collected.push({
      url,
      status,
      method: response.request().method(),
    });
  };

  page.on('response', listener);

  return {
    collect: () => [...collected],
    detach: () => {
      page.off('response', listener);
    },
    assertEmpty: () => {
      page.off('response', listener);
      if (collected.length === 0) {
        return;
      }
      const lines = collected.map((entry) => `  ${entry.method} ${entry.url} -> ${entry.status}`);
      throw new Error(
        `noNetworkErrors: ${collected.length} response(s) ≥${threshold}:\n${lines.join('\n')}`
      );
    },
  };
}

// ---------------------------------------------------------------------------
// Internal helpers — exported for unit-tests but not part of the public surface
// ---------------------------------------------------------------------------

export function matchesAllowlist(
  value: string,
  allowlist: ReadonlyArray<string | RegExp>
): boolean {
  for (const entry of allowlist) {
    if (typeof entry === 'string') {
      if (value.includes(entry)) {
        return true;
      }
    } else if (entry.test(value)) {
      return true;
    }
  }
  return false;
}

export function trimTrailingSlash(url: string): string {
  return url.endsWith('/') ? url.slice(0, -1) : url;
}

function assertIsObject(body: unknown, url: string): asserts body is Record<string, unknown> {
  if (body === null || typeof body !== 'object' || Array.isArray(body)) {
    throw new Error(`${url} response JSON was not an object: ${JSON.stringify(body)}`);
  }
}

function stringifyError(err: unknown): string {
  if (err instanceof Error) {
    return err.message;
  }
  return String(err);
}
