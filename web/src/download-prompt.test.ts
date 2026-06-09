import { describe, expect, it } from 'vitest';

import { buildDownloadRequest, isSendablePath, normalizeDownloadPath } from './download-prompt';

/**
 * Regression tests for the G8 (#100 / #87 B3) download-path logic that
 * replaced `window.prompt()` with the in-app #download-overlay modal.
 *
 * These pin the wire-level contract: the modal must keep sending the
 * exact `{ t: 'fdr', path }` message the agent already understands, and
 * must keep the prompt's old "empty entry = no-op" guard. A regression
 * here (e.g. sending a whitespace-only path, or changing the message
 * tag) would silently break the remote-file-download round-trip.
 */

describe('normalizeDownloadPath (#100 G8)', () => {
  it('trims leading and trailing whitespace', () => {
    expect(normalizeDownloadPath('  /home/user/file.txt  ')).toBe('/home/user/file.txt');
    expect(normalizeDownloadPath('\t report.pdf \n')).toBe('report.pdf');
  });

  it('leaves interior characters untouched', () => {
    // Paths legitimately contain spaces; only the edges are trimmed.
    expect(normalizeDownloadPath('  my docs/quarterly report.pdf  ')).toBe(
      'my docs/quarterly report.pdf'
    );
  });

  it('collapses a whitespace-only entry to empty', () => {
    expect(normalizeDownloadPath('    ')).toBe('');
    expect(normalizeDownloadPath('\t\n')).toBe('');
  });
});

describe('isSendablePath (#100 G8)', () => {
  it('accepts a non-empty normalized path', () => {
    expect(isSendablePath('/etc/hostname')).toBe(true);
    expect(isSendablePath('relative/path')).toBe(true);
  });

  it('rejects an empty path (mirrors the old `if (path && ...)` guard)', () => {
    expect(isSendablePath('')).toBe(false);
  });
});

describe('buildDownloadRequest (#100 G8)', () => {
  it('builds the exact { t: "fdr", path } message the agent expects', () => {
    expect(buildDownloadRequest('/home/user/report.pdf')).toEqual({
      t: 'fdr',
      path: '/home/user/report.pdf',
    });
  });

  it('trims before building so a fat-finger space does not break the lookup', () => {
    expect(buildDownloadRequest('   notes.txt  ')).toEqual({ t: 'fdr', path: 'notes.txt' });
  });

  it('returns null for an empty or whitespace-only entry (no message sent)', () => {
    expect(buildDownloadRequest('')).toBeNull();
    expect(buildDownloadRequest('   ')).toBeNull();
    expect(buildDownloadRequest('\t\n ')).toBeNull();
  });

  it('preserves interior spaces in the emitted path', () => {
    const event = buildDownloadRequest('  Desktop/my file.txt  ');
    expect(event).toEqual({ t: 'fdr', path: 'Desktop/my file.txt' });
  });
});
