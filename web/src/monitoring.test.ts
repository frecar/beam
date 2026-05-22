import * as Sentry from '@sentry/browser';
import { afterEach, describe, expect, it, vi } from 'vitest';

import {
  initMonitoring,
  scrubBreadcrumb,
  scrubEvent,
  scrubSensitiveValue,
  scrubUrl,
} from './monitoring';

vi.mock('@sentry/browser', () => ({
  init: vi.fn(),
}));

afterEach(() => {
  if (typeof window !== 'undefined') {
    delete window.__BEAM_RUNTIME_CONFIG__;
  }
  vi.unstubAllGlobals();
  vi.mocked(Sentry.init).mockClear();
});

describe('monitoring scrubber', () => {
  it('redacts session identifiers and sensitive query parameters from URLs', () => {
    expect(scrubUrl('/api/sessions/abc123/release?token=secret&view=desktop')).toBe(
      '/api/sessions/[session]/release?token=[redacted]&view=desktop'
    );
  });

  it('redacts nested sensitive fields without dropping safe diagnostics', () => {
    const scrubbed = scrubSensitiveValue({
      username: 'alice',
      viewport: { width: 1920, height: 1080 },
      nested: { release_token: 'secret-token' },
    });

    expect(scrubbed).toEqual({
      username: '[redacted]',
      viewport: { width: 1920, height: 1080 },
      nested: { release_token: '[redacted]' },
    });
  });

  it('removes user and request credentials from sentry events', () => {
    const event = scrubEvent({
      user: { username: 'alice' },
      request: {
        url: '/api/sessions/session-1/release',
        headers: {
          authorization: 'Bearer jwt',
          'x-request-id': 'req-1',
        },
        cookies: { beam_session: 'secret' },
        data: { password: 'secret', viewport_width: 1920 },
      },
      extra: { token: 'jwt', frame: 'decode' },
    });

    expect(event.user).toBeUndefined();
    expect(event.request?.url).toBe('/api/sessions/[session]/release');
    expect(event.request?.headers).toEqual({
      authorization: '[redacted]',
      'x-request-id': 'req-1',
    });
    expect(event.request?.cookies).toBeUndefined();
    expect(event.request?.data).toEqual({ password: '[redacted]', viewport_width: 1920 });
    expect(event.extra).toEqual({ token: '[redacted]', frame: 'decode' });
  });

  it('scrubs breadcrumb messages and data', () => {
    const breadcrumb = scrubBreadcrumb({
      category: 'fetch',
      message: '/api/sessions/abc123?release_token=secret',
      data: { session_id: 'abc123', status: 429 },
    });

    expect(breadcrumb.message).toBe('/api/sessions/[session]?release_token=[redacted]');
    expect(breadcrumb.data).toEqual({ session_id: '[redacted]', status: 429 });
  });
});

describe('initMonitoring', () => {
  it('does not initialize sentry without a runtime DSN', () => {
    vi.stubGlobal('window', {});
    window.__BEAM_RUNTIME_CONFIG__ = { observability: { sentryDsn: '' } };

    initMonitoring();

    expect(Sentry.init).not.toHaveBeenCalled();
  });

  it('initializes sentry from runtime config', () => {
    vi.stubGlobal('window', {});
    window.__BEAM_RUNTIME_CONFIG__ = {
      observability: {
        sentryDsn: ' https://public@example.invalid/1 ',
        sentryTracesSampleRate: 0.25,
        sentryEnvironment: 'production',
        release: '0.3.24',
      },
    };

    initMonitoring();

    expect(Sentry.init).toHaveBeenCalledOnce();
    expect(Sentry.init).toHaveBeenCalledWith(
      expect.objectContaining({
        dsn: 'https://public@example.invalid/1',
        environment: 'production',
        release: '0.3.24',
        sendDefaultPii: false,
        tracesSampleRate: 0.25,
      })
    );
  });
});
