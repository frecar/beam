import { describe, expect, it, vi } from 'vitest';
import type {
  APIRequestContext,
  ConsoleMessage,
  Page,
  Request as PWRequest,
  Response,
} from '@playwright/test';
import {
  healthcheck200,
  loginPasses,
  matchesAllowlist,
  recordConsoleErrors,
  recordNetworkErrors,
  trimTrailingSlash,
} from '../assertions';

// ---------------------------------------------------------------------------
// matchesAllowlist
// ---------------------------------------------------------------------------

describe('matchesAllowlist', () => {
  it('returns false for an empty allowlist', () => {
    expect(matchesAllowlist('anything', [])).toBe(false);
  });

  it('matches a string allowlist entry via substring', () => {
    expect(matchesAllowlist('https://sentry.example.com/api', ['sentry'])).toBe(true);
    expect(matchesAllowlist('https://other.example.com', ['sentry'])).toBe(false);
  });

  it('matches a RegExp allowlist entry', () => {
    expect(matchesAllowlist('TypeError: undefined', [/TypeError/])).toBe(true);
    expect(matchesAllowlist('ValueError: oops', [/TypeError/])).toBe(false);
  });

  it('matches if ANY entry matches', () => {
    expect(matchesAllowlist('sentry sample', ['noise', /sentry/])).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// trimTrailingSlash
// ---------------------------------------------------------------------------

describe('trimTrailingSlash', () => {
  it('removes a single trailing slash', () => {
    expect(trimTrailingSlash('http://localhost:8000/')).toBe('http://localhost:8000');
  });

  it('leaves URLs without trailing slash unchanged', () => {
    expect(trimTrailingSlash('http://localhost:8000')).toBe('http://localhost:8000');
  });

  it('only removes ONE trailing slash (the conventional case)', () => {
    expect(trimTrailingSlash('http://localhost:8000//')).toBe('http://localhost:8000/');
  });

  it('handles bare slash', () => {
    expect(trimTrailingSlash('/')).toBe('');
  });
});

// ---------------------------------------------------------------------------
// healthcheck200 — mock APIRequestContext
// ---------------------------------------------------------------------------

interface MockResponse {
  status: () => number;
  json: () => Promise<unknown>;
}

function mockRequest(response: MockResponse): APIRequestContext {
  return {
    get: vi.fn(async () => response),
  } as unknown as APIRequestContext;
}

describe('healthcheck200', () => {
  it('passes when /health returns 200 with default keys', async () => {
    const request = mockRequest({
      status: () => 200,
      json: async () => ({ status: 'ok' }),
    });
    await expect(healthcheck200(request, 'http://localhost:8222')).resolves.toBeDefined();
  });

  it('throws when status is not 200', async () => {
    const request = mockRequest({
      status: () => 503,
      json: async () => ({}),
    });
    await expect(healthcheck200(request, 'http://localhost:8222')).rejects.toThrow(
      /did not return 200/
    );
  });

  it('uses the configured path override', async () => {
    const getMock = vi.fn(async () => ({
      status: () => 200,
      json: async () => ({ status: 'ok' }),
    }));
    const request = { get: getMock } as unknown as APIRequestContext;
    await healthcheck200(request, 'http://localhost:8000', { path: '/api/health' });
    expect(getMock).toHaveBeenCalledWith('http://localhost:8000/api/health');
  });

  it('trims trailing slash on baseURL before joining', async () => {
    const getMock = vi.fn(async () => ({
      status: () => 200,
      json: async () => ({ status: 'ok' }),
    }));
    const request = { get: getMock } as unknown as APIRequestContext;
    await healthcheck200(request, 'http://localhost:8000/', { path: '/health' });
    expect(getMock).toHaveBeenCalledWith('http://localhost:8000/health');
  });

  it('throws when JSON body is missing the default status key', async () => {
    const request = mockRequest({
      status: () => 200,
      json: async () => ({ ok: true }),
    });
    await expect(healthcheck200(request, 'http://localhost:8222')).rejects.toThrow(
      /missing expected key 'status'/
    );
  });

  it('throws when JSON parse fails', async () => {
    const request = mockRequest({
      status: () => 200,
      json: async () => {
        throw new Error('Unexpected token <');
      },
    });
    await expect(healthcheck200(request, 'http://localhost:8222')).rejects.toThrow(
      /did not parse as JSON/
    );
  });

  it('throws when body is not an object', async () => {
    const request = mockRequest({
      status: () => 200,
      json: async () => 'just a string',
    });
    await expect(healthcheck200(request, 'http://localhost:8222')).rejects.toThrow(
      /response JSON was not an object/
    );
  });

  it('throws when body is an array', async () => {
    const request = mockRequest({
      status: () => 200,
      json: async () => [{ status: 'ok' }],
    });
    await expect(healthcheck200(request, 'http://localhost:8222')).rejects.toThrow(
      /response JSON was not an object/
    );
  });

  it('throws when body is null', async () => {
    const request = mockRequest({
      status: () => 200,
      json: async () => null,
    });
    await expect(healthcheck200(request, 'http://localhost:8222')).rejects.toThrow(
      /response JSON was not an object/
    );
  });

  it('skips body shape check when expectedKeys is empty', async () => {
    const request = mockRequest({
      status: () => 200,
      json: async () => {
        throw new Error('should not be called');
      },
    });
    await expect(
      healthcheck200(request, 'http://localhost:8222', { expectedKeys: [] })
    ).resolves.toBeUndefined();
  });

  it('passes when expectedValues match', async () => {
    const request = mockRequest({
      status: () => 200,
      json: async () => ({ status: 'ok', version: '1.2.3' }),
    });
    await expect(
      healthcheck200(request, 'http://localhost:8222', {
        expectedKeys: ['status', 'version'],
        expectedValues: { status: 'ok' },
      })
    ).resolves.toBeDefined();
  });

  it('throws when expectedValues mismatch', async () => {
    const request = mockRequest({
      status: () => 200,
      json: async () => ({ status: 'degraded' }),
    });
    await expect(
      healthcheck200(request, 'http://localhost:8222', {
        expectedValues: { status: 'ok' },
      })
    ).rejects.toThrow(/expected "ok"/);
  });

  it('returns the parsed body for downstream assertions', async () => {
    const body = { status: 'ok', services: ['a', 'b'] };
    const request = mockRequest({
      status: () => 200,
      json: async () => body,
    });
    const result = await healthcheck200(request, 'http://localhost:8222');
    expect(result).toEqual(body);
  });
});

// ---------------------------------------------------------------------------
// loginPasses — mock Page
// ---------------------------------------------------------------------------

function mockPage(opts: { url: string; locatorFactory?: (selector: string) => unknown }): Page {
  return {
    url: () => opts.url,
    locator:
      opts.locatorFactory ??
      ((_selector: string) => ({
        // expect(locator).toBeVisible / toContainText resolve immediately in tests
      })),
  } as unknown as Page;
}

describe('loginPasses', () => {
  it('passes when URL no longer contains login fragment', async () => {
    const page = mockPage({ url: 'http://localhost:8000/dashboard' });
    await expect(loginPasses(page)).resolves.toBeUndefined();
  });

  it('throws when URL still contains default login fragment', async () => {
    const page = mockPage({ url: 'http://localhost:8000/login' });
    await expect(loginPasses(page)).rejects.toThrow(/URL still contains login fragment/);
  });

  it('honours custom loginPathFragment', async () => {
    const page = mockPage({ url: 'http://localhost:8000/if/flow/authentik/' });
    await expect(loginPasses(page, { loginPathFragment: '/if/flow' })).rejects.toThrow(
      /\/if\/flow/
    );
  });

  it('matches portal /local-login/ via custom fragment (repo overrides default)', async () => {
    // /login default does NOT substring-match /local-login/ — portal configures
    // the fragment explicitly. Documents the boundary.
    const page = mockPage({ url: 'http://localhost:8000/local-login/' });
    await expect(loginPasses(page, { loginPathFragment: 'local-login' })).rejects.toThrow(
      /URL still contains/
    );
  });

  it('default /login fragment does NOT match /local-login/', async () => {
    const page = mockPage({ url: 'http://localhost:8000/local-login/' });
    await expect(loginPasses(page)).resolves.toBeUndefined();
  });

  it('returns ok when only URL check requested', async () => {
    const page = mockPage({ url: 'http://localhost:8000/' });
    await expect(loginPasses(page)).resolves.toBeUndefined();
  });
});

// ---------------------------------------------------------------------------
// recordConsoleErrors — mock Page event emitter
// ---------------------------------------------------------------------------

interface FakePage {
  page: Page;
  fireConsole: (msg: Partial<ConsoleMessage> & { type: string; text: string }) => void;
  listenerCount: () => number;
}

function makeFakePage(): FakePage {
  let listener: ((msg: ConsoleMessage) => void) | undefined;
  let responseListener: ((res: Response) => void) | undefined;

  const page = {
    on: (event: string, fn: (...args: unknown[]) => void) => {
      if (event === 'console') {
        listener = fn as (msg: ConsoleMessage) => void;
      } else if (event === 'response') {
        responseListener = fn as (res: Response) => void;
      }
    },
    off: (event: string, _fn: (...args: unknown[]) => void) => {
      if (event === 'console') {
        listener = undefined;
      } else if (event === 'response') {
        responseListener = undefined;
      }
    },
  } as unknown as Page;

  return {
    page,
    fireConsole: (msg) => {
      const fakeLocation = {
        url: 'http://localhost:8000/main.js',
        lineNumber: 10,
        columnNumber: 20,
      };
      const full = {
        type: () => msg.type,
        text: () => msg.text,
        location: () => fakeLocation,
      } as unknown as ConsoleMessage;
      listener?.(full);
    },
    listenerCount: () =>
      (listener !== undefined ? 1 : 0) + (responseListener !== undefined ? 1 : 0),
  };
}

describe('recordConsoleErrors', () => {
  it('captures error-level console messages by default', () => {
    const fake = makeFakePage();
    const recorder = recordConsoleErrors(fake.page);

    fake.fireConsole({ type: 'error', text: 'Uncaught: oops' });
    fake.fireConsole({ type: 'warning', text: 'soft warn' });

    expect(recorder.collect()).toHaveLength(1);
    expect(recorder.collect()[0]?.text).toBe('Uncaught: oops');
  });

  it('captures warnings when level set includes warning', () => {
    const fake = makeFakePage();
    const recorder = recordConsoleErrors(fake.page, { levels: ['error', 'warning'] });

    fake.fireConsole({ type: 'error', text: 'err' });
    fake.fireConsole({ type: 'warning', text: 'warn' });
    fake.fireConsole({ type: 'log', text: 'noise' });

    expect(recorder.collect()).toHaveLength(2);
  });

  it('ignores allowlisted patterns', () => {
    const fake = makeFakePage();
    const recorder = recordConsoleErrors(fake.page, {
      allowlist: [/Sentry/, 'CSP report-only'],
    });

    fake.fireConsole({ type: 'error', text: 'Sentry failed to fetch' });
    fake.fireConsole({ type: 'error', text: 'CSP report-only blocked' });
    fake.fireConsole({ type: 'error', text: 'actual app error' });

    expect(recorder.collect()).toHaveLength(1);
    expect(recorder.collect()[0]?.text).toBe('actual app error');
  });

  it('assertEmpty passes when no errors captured', () => {
    const fake = makeFakePage();
    const recorder = recordConsoleErrors(fake.page);
    expect(() => recorder.assertEmpty()).not.toThrow();
  });

  it('assertEmpty throws with aggregated message when errors present', () => {
    const fake = makeFakePage();
    const recorder = recordConsoleErrors(fake.page);
    fake.fireConsole({ type: 'error', text: 'first' });
    fake.fireConsole({ type: 'error', text: 'second' });

    expect(() => recorder.assertEmpty()).toThrow(/2 console error\(s\) emitted/);
    expect(() => recorder.assertEmpty()).toThrow(/first/);
  });

  it('detach removes the listener', () => {
    const fake = makeFakePage();
    const recorder = recordConsoleErrors(fake.page);
    expect(fake.listenerCount()).toBe(1);
    recorder.detach();
    expect(fake.listenerCount()).toBe(0);
  });

  it('assertEmpty also detaches the listener', () => {
    const fake = makeFakePage();
    const recorder = recordConsoleErrors(fake.page);
    expect(fake.listenerCount()).toBe(1);
    recorder.assertEmpty();
    expect(fake.listenerCount()).toBe(0);
  });

  it('collect returns a copy (mutation isolation)', () => {
    const fake = makeFakePage();
    const recorder = recordConsoleErrors(fake.page);
    fake.fireConsole({ type: 'error', text: 'one' });

    const snapshot = recorder.collect();
    snapshot.push({ type: 'error', text: 'mutated', location: 'fake' });
    expect(recorder.collect()).toHaveLength(1);
  });
});

// ---------------------------------------------------------------------------
// recordNetworkErrors — mock Page response stream
// ---------------------------------------------------------------------------

interface FakeNetworkPage {
  page: Page;
  fireResponse: (opts: { url: string; status: number; method?: string }) => void;
}

function makeFakeNetworkPage(): FakeNetworkPage {
  let responseListener: ((res: Response) => void) | undefined;

  const page = {
    on: (event: string, fn: (...args: unknown[]) => void) => {
      if (event === 'response') {
        responseListener = fn as (res: Response) => void;
      }
    },
    off: (event: string, _fn: (...args: unknown[]) => void) => {
      if (event === 'response') {
        responseListener = undefined;
      }
    },
  } as unknown as Page;

  return {
    page,
    fireResponse: (opts) => {
      const fakeRequest = {
        method: () => opts.method ?? 'GET',
      } as unknown as PWRequest;
      const res = {
        url: () => opts.url,
        status: () => opts.status,
        request: () => fakeRequest,
      } as unknown as Response;
      responseListener?.(res);
    },
  };
}

describe('recordNetworkErrors', () => {
  it('captures 5xx responses by default', () => {
    const fake = makeFakeNetworkPage();
    const rec = recordNetworkErrors(fake.page);
    fake.fireResponse({ url: 'http://localhost:8000/api/foo', status: 500 });
    fake.fireResponse({ url: 'http://localhost:8000/api/bar', status: 200 });
    expect(rec.collect()).toHaveLength(1);
    expect(rec.collect()[0]?.status).toBe(500);
  });

  it('ignores 4xx by default', () => {
    const fake = makeFakeNetworkPage();
    const rec = recordNetworkErrors(fake.page);
    fake.fireResponse({ url: 'http://localhost:8000/api/foo', status: 404 });
    expect(rec.collect()).toHaveLength(0);
  });

  it('captures 4xx when threshold is lowered', () => {
    const fake = makeFakeNetworkPage();
    const rec = recordNetworkErrors(fake.page, { failOnStatusGte: 400 });
    fake.fireResponse({ url: 'http://localhost:8000/api/foo', status: 404 });
    expect(rec.collect()).toHaveLength(1);
  });

  it('scopes to origin when scopeToOrigin is set', () => {
    const fake = makeFakeNetworkPage();
    const rec = recordNetworkErrors(fake.page, {
      scopeToOrigin: 'http://localhost:8000',
    });
    fake.fireResponse({ url: 'http://localhost:8000/api/foo', status: 500 });
    fake.fireResponse({ url: 'https://cdn.example.com/lib.js', status: 503 });
    expect(rec.collect()).toHaveLength(1);
    expect(rec.collect()[0]?.url).toContain('localhost');
  });

  it('honours URL allowlist', () => {
    const fake = makeFakeNetworkPage();
    const rec = recordNetworkErrors(fake.page, {
      allowlist: [/sentry/, '/known-noisy'],
    });
    fake.fireResponse({ url: 'https://sentry.example.com/api/sample', status: 503 });
    fake.fireResponse({ url: 'http://localhost:8000/known-noisy', status: 503 });
    fake.fireResponse({ url: 'http://localhost:8000/api/foo', status: 503 });
    expect(rec.collect()).toHaveLength(1);
    expect(rec.collect()[0]?.url).toContain('/api/foo');
  });

  it('captures request method', () => {
    const fake = makeFakeNetworkPage();
    const rec = recordNetworkErrors(fake.page);
    fake.fireResponse({ url: 'http://localhost:8000/api/foo', status: 502, method: 'POST' });
    expect(rec.collect()[0]?.method).toBe('POST');
  });

  it('assertEmpty passes when no failures captured', () => {
    const fake = makeFakeNetworkPage();
    const rec = recordNetworkErrors(fake.page);
    expect(() => rec.assertEmpty()).not.toThrow();
  });

  it('assertEmpty throws aggregated message on failure', () => {
    const fake = makeFakeNetworkPage();
    const rec = recordNetworkErrors(fake.page);
    fake.fireResponse({ url: 'http://localhost:8000/api/a', status: 500 });
    fake.fireResponse({ url: 'http://localhost:8000/api/b', status: 503, method: 'POST' });
    expect(() => rec.assertEmpty()).toThrow(/2 response\(s\) ≥500/);
    expect(() => rec.assertEmpty()).toThrow(/POST http:\/\/localhost:8000\/api\/b/);
  });

  it('detach + collect returns snapshot', () => {
    const fake = makeFakeNetworkPage();
    const rec = recordNetworkErrors(fake.page);
    fake.fireResponse({ url: 'http://localhost:8000/api', status: 500 });
    rec.detach();
    expect(rec.collect()).toHaveLength(1);
  });
});
