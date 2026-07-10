# Browser E2E smoke tests

Playwright smoke suite for the beam web client. It exercises a shared
contract — the app boots cleanly in a real browser, a health probe returns
the expected JSON shape, and the login surface renders without console or
network errors — with **all** backend and external calls mocked via
`page.route`. No real signaling server, agent, or external service is
touched.

## Layout

| Path                     | What it is                                                                                                  |
| ------------------------ | ----------------------------------------------------------------------------------------------------------- |
| `smoke.spec.ts`          | The beam-specific smoke spec. Playwright runs `*.spec.ts` only.                                             |
| `shared/assertions.ts`   | Reusable assertion helpers (`healthcheck200`, `loginPasses`, `recordConsoleErrors`, `recordNetworkErrors`). |
| `shared/config.ts`       | `defineSmokeConfig` — shared Playwright defaults (chromium, traces, screenshots, timeouts).                 |
| `shared/index.ts`        | Public entry point re-exporting the helpers + config.                                                       |
| `shared/tests/*.test.ts` | Unit tests for the vendored helpers. Run by **vitest** (`*.test.ts`), not Playwright.                       |

`shared/` is **vendored** — copied, not symlinked, from a shared
smoke-test foundation. Refresh by re-copying at vendor-bump time. The
helpers are framework-generic; the beam-specific page navigation lives in
`smoke.spec.ts`.

## Running

```bash
# From web/ — install the browser once, then run the suite.
npm run test:e2e:install   # playwright install --with-deps chromium
npm run test:e2e           # playwright test

# Or via the repo Makefile:
make e2e
```

The Playwright config builds the production bundle and serves it with
`vite preview` on `127.0.0.1:<per-run port>` (resolved from `GITHUB_RUN_ID`
by the shared config with `--strictPort`; 4173 locally), then drives chromium
against it.

## Conventions

- **Vitest** owns `*.test.ts` (helper unit tests, fast, no browser).
- **Playwright** owns `*.spec.ts` (browser smoke, slower).
  `playwright.config.ts` sets `testMatch: '**/*.spec.ts'` and
  `testIgnore: '**/shared/**'` so the two never collide.
- The vendored `shared/` helpers are excluded from the `src/` coverage
  ratchet (`coverage.include: ['src/**/*.ts']` in `vite.config.ts`), but
  their behaviour is still validated by `shared/tests/*.test.ts` under
  vitest.

## Why `/health` is mocked, not hit for real

In production `/health` is served by the Rust signaling server, not the
Vite preview server that hosts this SPA. A direct probe against the
preview would receive the SPA's `index.html` (SPA fallback), not JSON.
The suite mocks `/health` at the browser layer and exercises the shared
`healthcheck200` contract against that mocked response, keeping the smoke
build-time and hermetic. End-to-end coverage of the real server + UI
together belongs in a separate harness run against a live stack.
