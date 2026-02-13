/**
 * Platform detection and Mac keyboard remapping.
 *
 * On macOS, Cmd is the primary modifier for shortcuts. On Linux, Ctrl fills
 * that role. We remap ALL Cmd+key combos to Ctrl+key so the remote desktop
 * behaves naturally for Mac users. The Meta/Super key itself is never sent
 * to the remote (it would trigger window manager actions).
 *
 * Browser-level shortcuts (Cmd+T, Cmd+W, etc.) are left alone so the
 * browser still functions normally.
 */

export const isMac: boolean =
  typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform);

/** Browser shortcuts that should NOT be intercepted (let the browser handle them) */
const BROWSER_SHORTCUTS = new Set([
  "l", "n", "q", "t", "w", "shift+t",
]);

export function isBrowserShortcut(e: KeyboardEvent): boolean {
  if (isMac) {
    if (!e.metaKey) return false;
  } else {
    if (!e.ctrlKey) return false;
  }
  const key = e.key.toLowerCase();
  if (e.shiftKey) return BROWSER_SHORTCUTS.has(`shift+${key}`);
  return BROWSER_SHORTCUTS.has(key);
}
