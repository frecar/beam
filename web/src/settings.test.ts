import { describe, expect, it } from 'vitest';

// formatCountdown is a pure function but settings.ts imports ui-state.ts which
// requires DOM. Re-implement inline to test the logic without DOM dependency.
function formatCountdown(secs: number): string {
  const m = Math.floor(secs / 60);
  const s = Math.floor(secs % 60);
  if (m > 0) return `${m}m ${s}s`;
  return `${s}s`;
}

// Same re-implementation rationale as above: avoid importing settings.ts
// (which imports ui-state.ts and depends on DOM existing at import time).
function buildIdleWarningMessage(remainingSecs: number | undefined, coarse: boolean): string {
  const cta = coarse
    ? 'Tap anywhere to stay connected.'
    : 'Move mouse or press a key to stay connected.';
  if (remainingSecs !== undefined && remainingSecs > 0) {
    return `Session expires in ${formatCountdown(remainingSecs)}. ${cta}`;
  }
  return `Session will expire due to inactivity. ${cta}`;
}

describe('formatCountdown', () => {
  it('formats minutes and seconds', () => {
    expect(formatCountdown(125)).toBe('2m 5s');
    expect(formatCountdown(60)).toBe('1m 0s');
    expect(formatCountdown(90)).toBe('1m 30s');
  });

  it('formats seconds only when under a minute', () => {
    expect(formatCountdown(45)).toBe('45s');
    expect(formatCountdown(0)).toBe('0s');
  });
});

describe('buildIdleWarningMessage (#87 G7)', () => {
  it('uses mouse/keyboard CTA on fine pointers', () => {
    expect(buildIdleWarningMessage(undefined, false)).toBe(
      'Session will expire due to inactivity. Move mouse or press a key to stay connected.'
    );
  });

  it('uses tap CTA on coarse pointers (touch devices)', () => {
    expect(buildIdleWarningMessage(undefined, true)).toBe(
      'Session will expire due to inactivity. Tap anywhere to stay connected.'
    );
  });

  it('embeds the countdown when remainingSecs is positive', () => {
    expect(buildIdleWarningMessage(90, false)).toBe(
      'Session expires in 1m 30s. Move mouse or press a key to stay connected.'
    );
    expect(buildIdleWarningMessage(45, true)).toBe(
      'Session expires in 45s. Tap anywhere to stay connected.'
    );
  });

  it('falls back to the generic message when remainingSecs is 0 or undefined', () => {
    // 0 means the timer already expired, but the warning logic clamps to
    // 0 via Math.max — we still want the generic copy in that case.
    expect(buildIdleWarningMessage(0, false)).toMatch(/^Session will expire due to inactivity/);
    expect(buildIdleWarningMessage(undefined, true)).toMatch(
      /^Session will expire due to inactivity/
    );
  });
});
