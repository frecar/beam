import { describe, expect, it } from 'vitest';

import { scrubBreadcrumb, scrubEvent, scrubSensitiveValue, scrubUrl } from './monitoring';

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
