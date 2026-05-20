import type { Breadcrumb, ErrorEvent, Event } from '@sentry/browser';
import * as Sentry from '@sentry/browser';

const REDACTED = '[redacted]';
const MAX_DEPTH = 6;
const SENSITIVE_KEY =
  /(authorization|cookie|jwt|password|release[_-]?token|secret|session[_-]?id|token|username)/i;
const SENSITIVE_QUERY = /([?&](?:password|release_token|session_id|token|username)=)[^&#]*/gi;

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

export function scrubUrl(url: string | undefined): string | undefined {
  if (!url) return url;
  return url
    .replace(/\/api\/sessions\/[^/?#]+/g, '/api/sessions/[session]')
    .replace(SENSITIVE_QUERY, `$1${REDACTED}`);
}

export function scrubSensitiveValue(value: unknown, key = '', depth = MAX_DEPTH): unknown {
  if (SENSITIVE_KEY.test(key)) return REDACTED;
  if (depth <= 0) return '[truncated]';
  if (typeof value === 'string') return value.replace(SENSITIVE_QUERY, `$1${REDACTED}`);
  if (Array.isArray(value)) {
    return value.map((item) => scrubSensitiveValue(item, '', depth - 1));
  }
  if (!isRecord(value)) return value;

  return Object.fromEntries(
    Object.entries(value).map(([entryKey, entryValue]) => [
      entryKey,
      scrubSensitiveValue(entryValue, entryKey, depth - 1),
    ])
  );
}

type RequestEventData = NonNullable<Event['request']>;

function scrubRequest(request: RequestEventData | undefined): RequestEventData | undefined {
  if (!request) return request;
  const scrubbed = {
    ...request,
    url: scrubUrl(request.url),
    headers: scrubSensitiveValue(request.headers) as RequestEventData['headers'],
    data: scrubSensitiveValue(request.data),
  };
  delete scrubbed.cookies;
  return scrubbed;
}

export function scrubEvent<TEvent extends Event>(event: TEvent): TEvent {
  return {
    ...event,
    user: undefined,
    request: scrubRequest(event.request),
    contexts: scrubSensitiveValue(event.contexts) as Event['contexts'],
    extra: scrubSensitiveValue(event.extra) as Event['extra'],
    tags: scrubSensitiveValue(event.tags) as Event['tags'],
  } as TEvent;
}

export function scrubErrorEvent(event: ErrorEvent): ErrorEvent {
  return scrubEvent(event);
}

export function scrubBreadcrumb(breadcrumb: Breadcrumb): Breadcrumb {
  return {
    ...breadcrumb,
    message:
      typeof breadcrumb.message === 'string' ? scrubUrl(breadcrumb.message) : breadcrumb.message,
    data: scrubSensitiveValue(breadcrumb.data) as Breadcrumb['data'],
  };
}

function numberFromEnv(value: string | undefined): number | undefined {
  if (!value) return undefined;
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : undefined;
}

export function initMonitoring(): void {
  const dsn = import.meta.env.VITE_SENTRY_DSN?.trim();
  if (!dsn) return;

  Sentry.init({
    dsn,
    environment: import.meta.env.MODE,
    release: import.meta.env.VITE_BEAM_RELEASE,
    sendDefaultPii: false,
    tracesSampleRate: numberFromEnv(import.meta.env.VITE_SENTRY_TRACES_SAMPLE_RATE) ?? 0,
    beforeSend: scrubErrorEvent,
    beforeBreadcrumb: scrubBreadcrumb,
  });
}
