import { expect, type Page, type Route, test } from '@playwright/test';

import { loginPasses, recordConsoleErrors, recordNetworkErrors } from './shared';

// Shared browser smoke contract for the beam web client.
//
// The suite is hermetic: every request is intercepted via `page.route`,
// so no real backend, agent, or external service is touched. Mocked
// responses guarantee no real 5xx; the console + network recorders catch
// frontend-side regressions (a runtime error on first paint, a burst-fetch
// to a missing endpoint, etc.).
//
// The web client is a single-page login UI served by Vite's preview
// server. `/health` is served by the Rust signaling server in production,
// NOT the preview server — so we mock it at the browser layer and exercise
// the shared `healthcheck200` contract against the mocked response via
// `page.request` (which honours `page.route` handlers).

const HEALTH_BODY = {
  status: 'ok',
  timestamp: '2026-05-30T12:00:00Z',
  version: 'smoke',
};

/**
 * Intercept every request. Mock the health probe and any `/api/*` call so
 * the suite never reaches a real backend; pass asset bytes through to the
 * preview server.
 */
async function routeBackend(page: Page): Promise<void> {
  await page.route('**/*', async (route: Route) => {
    const request = route.request();
    const url = new URL(request.url());

    if (url.pathname === '/health') {
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify(HEALTH_BODY),
      });
      return;
    }

    if (url.pathname === '/api/auth/login' && request.method() === 'POST') {
      // A successful auth response shape. The desktop handoff that follows
      // needs a live streaming agent, which is out of scope for a
      // build-time smoke suite — we assert the login call itself is clean.
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({
          session_id: 'smoke-session',
          token: 'smoke-token',
          ws_url: 'wss://127.0.0.1:4173/ws',
        }),
      });
      return;
    }

    if (url.pathname.startsWith('/api/')) {
      await route.fulfill({
        status: 200,
        contentType: 'application/json',
        body: JSON.stringify({}),
      });
      return;
    }

    // Static assets (index.html, JS, CSS, fonts) — let the preview serve them.
    await route.continue();
  });
}

test.beforeEach(async ({ page }) => {
  await routeBackend(page);
});

test('login UI renders without console or network errors', async ({ page, baseURL }) => {
  const networkRecorder = recordNetworkErrors(page, { scopeToOrigin: baseURL ?? '' });
  const consoleRecorder = recordConsoleErrors(page);

  await page.goto('/');

  // The login card is the application's entry surface.
  await expect(page.getByRole('heading', { name: 'Beam' })).toBeVisible();
  await expect(page.getByText('Sign in with your Linux account')).toBeVisible();
  await expect(page.locator('#login-form')).toBeVisible();
  await expect(page.locator('#username')).toBeVisible();
  await expect(page.locator('#password')).toBeVisible();
  await expect(page.locator('#connect-btn')).toBeVisible();

  // Shared contract: zero same-origin 5xx + zero console errors on load.
  networkRecorder.assertEmpty();
  consoleRecorder.assertEmpty();
});

test('health probe returns the expected JSON contract', async ({ page }) => {
  // The shared `healthcheck200` helper takes an APIRequestContext, which
  // does NOT pass through `page.route` — so we cannot point it at the
  // preview server and have the mock honoured (it would hit the SPA
  // fallback and receive index.html). In production `/health` is served by
  // the Rust signaling server, not this Vite bundle, so probing the
  // preview for real JSON is not meaningful either.
  //
  // The `healthcheck200` helper itself is exercised against a mocked
  // APIRequestContext in the vendored unit tests
  // (`e2e/shared/tests/assertions.test.ts`, run under vitest). Here we
  // verify the same `/health` JSON contract end-to-end through the app's
  // own (mocked) network path: the browser's `fetch` IS intercepted by
  // `page.route`, so this asserts the shared shape with externals mocked.
  await page.goto('/');

  const body = await page.evaluate(async () => {
    const response = await fetch('/health');
    return { status: response.status, json: (await response.json()) as Record<string, unknown> };
  });

  expect(body.status).toBe(200);
  expect(body.json).toMatchObject({ status: 'ok', version: 'smoke' });
});

test('loginPasses reports the user is still gated on the login surface', async ({ page }) => {
  await page.goto('/');

  // Before authenticating, the login form is present. This is a single-page
  // app with no URL routing, so we assert the gate via the visible form
  // rather than a URL fragment, and confirm the shared helper agrees the
  // base URL is not itself a `/login`-style route (beam serves the SPA at
  // `/`, so the negative URL check passes trivially).
  await expect(page.locator('#login-form')).toBeVisible();
  await loginPasses(page, { loginPathFragment: '/login' });
});

test('login submit issues a clean auth request', async ({ page, baseURL }) => {
  const networkRecorder = recordNetworkErrors(page, { scopeToOrigin: baseURL ?? '' });

  await page.goto('/');
  await page.locator('#username').fill('smoke-user');
  await page.locator('#password').fill('smoke-pass');

  const [loginResponse] = await Promise.all([
    page.waitForResponse((res) => res.url().endsWith('/api/auth/login')),
    page.locator('#connect-btn').click(),
  ]);

  expect(loginResponse.status()).toBe(200);
  // The mocked auth succeeds; the subsequent streaming handoff cannot
  // complete without a live agent, so we only assert the auth round-trip
  // itself produced no same-origin 5xx.
  networkRecorder.assertEmpty();
});
