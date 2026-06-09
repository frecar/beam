/**
 * File-download path entry (audit G8 / B3).
 *
 * Replaces the old `window.prompt()` call in the download flow with an
 * in-app modal. The native prompt is hostile on touch: iOS Safari auto-
 * zooms, offers no styling, and is a single-line system dialog. The modal
 * in `index.html` (#download-overlay) gives a styled, 16px-font input
 * (the iOS auto-zoom threshold) that the rest of the client can theme and
 * size to the WCAG touch-target floor.
 *
 * This module holds the *pure* logic — normalizing the typed path and
 * deciding whether it's a sendable request — so it can be unit-tested
 * without a DOM. The thin DOM wiring (open/close/focus/submit) lives in
 * `main.ts`, which is excluded from coverage like every other entry-point
 * wiring file.
 *
 * Semantics are deliberately unchanged from the prompt: the value is a
 * path on the REMOTE desktop's filesystem (relative to home or absolute).
 * The agent reads that path and streams the file back via the existing
 * `{ t: 'fdr', path }` control message. This is NOT the browser File
 * System Access API (`showSaveFilePicker`), which would pick a *local*
 * save destination — a different concern that would not replace the
 * remote source-path selection this flow performs.
 */

import type { InputEvent } from './connection';

/**
 * The file-download-request control message — the `fdr` arm of the
 * {@link InputEvent} union, narrowed so callers can read `.path` directly
 * without a discriminant check. `Extract` keeps `InputEvent` the single
 * source of truth (this stays assignable to it).
 */
export type DownloadRequest = Extract<InputEvent, { t: 'fdr' }>;

/**
 * Normalize a raw path typed into the download input.
 *
 * Trims surrounding whitespace (a leading/trailing space from a phone
 * keyboard's autocorrect is never meaningful for a filesystem path and
 * would otherwise produce a "no such file" round-trip). Returns the
 * trimmed string; callers decide emptiness via {@link isSendablePath}.
 */
export function normalizeDownloadPath(raw: string): string {
  return raw.trim();
}

/**
 * Whether a normalized path is worth sending.
 *
 * Mirrors the old `if (path && connection)` guard: an empty or
 * whitespace-only entry is a no-op (the user cancelled by clearing the
 * field). Keeping this as its own predicate makes the modal's submit
 * handler a one-liner and gives the regression test a clear contract.
 */
export function isSendablePath(normalized: string): boolean {
  return normalized.length > 0;
}

/**
 * Build the file-download-request control message for a normalized path.
 *
 * Centralizes the `{ t: 'fdr', path }` shape so the modal wiring and the
 * test agree on exactly what crosses the wire. Returns `null` for a
 * non-sendable (empty) path so callers can `if (!event) return;` instead
 * of duplicating the emptiness check.
 */
export function buildDownloadRequest(rawPath: string): DownloadRequest | null {
  const path = normalizeDownloadPath(rawPath);
  if (!isSendablePath(path)) {
    return null;
  }
  return { t: 'fdr', path };
}
