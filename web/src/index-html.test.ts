import { describe, expect, it } from 'vitest';

// Vite resolves the `?raw` suffix at build/transform time, returning the
// file contents as a string. No node `fs`/`path`/`__dirname` reach for —
// keeps the test environment-agnostic.
import indexHtml from '../index.html?raw';

/**
 * Smoke tests over the inline `<style>` block in `web/index.html`.
 *
 * The status bar packs many controls into a narrow strip; on viewports
 * between the mobile cut-off (768px, where the bar is hidden entirely
 * via a separate media query) and full desktop widths (~1100px), the
 * button labels would push controls off the right edge with no
 * flex-wrap. The audit at `web/docs/mobile-ux-audit-2026-Q2.md` calls
 * this Blocker B2.
 *
 * Fix landed in this change: a `min-width:769px and max-width:1100px`
 * media query that hides `.btn-label` spans (icons + `aria-label`
 * remain for assistive tech). These assertions exist to prevent the
 * rule from silently regressing during future style sweeps — a
 * regression would re-introduce the overflow blocker.
 */

describe('index.html — narrow-desktop status bar (#87 B2)', () => {
  it('declares the 769-1100px media query block', () => {
    expect(indexHtml).toMatch(/@media\s*\(min-width:\s*769px\)\s*and\s*\(max-width:\s*1100px\)/);
  });

  it('hides .btn-label inside #status-bar within that block', () => {
    // Anchor on the media query opener so we don't false-positive on
    // unrelated `.btn-label { display: none }` rules elsewhere.
    const block = indexHtml.match(
      /@media\s*\(min-width:\s*769px\)\s*and\s*\(max-width:\s*1100px\)\s*\{([\s\S]*?)\n\s{6}\}/
    );
    expect(block).not.toBeNull();
    if (block !== null) {
      expect(block[1]).toMatch(/#status-bar\s+\.btn-label\s*\{[^}]*display:\s*none/);
    }
  });

  it('keeps every .status-btn with btn-label paired with an aria-label', () => {
    // Hiding the label is only accessible if the button itself names
    // itself via aria-label. Audit every status-btn that uses a label.
    const btnRegex = /<button[^>]*class="[^"]*\bstatus-btn\b[^"]*"[^>]*>[\s\S]*?<\/button>/g;
    const buttons = indexHtml.match(btnRegex) ?? [];
    expect(buttons.length).toBeGreaterThan(0);
    for (const btn of buttons) {
      if (btn.includes('btn-label')) {
        expect(btn).toMatch(/aria-label="[^"]+"/);
      }
    }
  });

  it('still hides the entire status bar below 768px (B1 cascade unchanged)', () => {
    // Sanity: this PR fixes B2 only. B1's `#status-bar.visible { display: none }`
    // inside `@media (max-width: 768px)` must remain — the mobile-FAB path
    // depends on it. A future PR will address B1 separately.
    expect(indexHtml).toMatch(
      /@media\s*\(max-width:\s*768px\)[\s\S]*?#status-bar\.visible\s*\{\s*display:\s*none/
    );
  });
});
