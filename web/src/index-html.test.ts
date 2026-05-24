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

describe('index.html — login form mobile polish (#87 G4)', () => {
  it('username input declares autocapitalize=none + enterkeyhint=next', () => {
    // iOS auto-capitalizes form inputs by default. Linux usernames are
    // lowercase; auto-cap forces the user to manually shift-lower the
    // first character. enterkeyhint=next cues the "Next" return key
    // instead of generic "Go".
    const usernameMatch = indexHtml.match(/<input[^>]*id="username"[^>]*>/);
    expect(usernameMatch).not.toBeNull();
    if (usernameMatch !== null) {
      expect(usernameMatch[0]).toMatch(/autocapitalize="none"/);
      expect(usernameMatch[0]).toMatch(/enterkeyhint="next"/);
    }
  });

  it('password input declares enterkeyhint=go', () => {
    const passwordMatch = indexHtml.match(/<input[^>]*id="password"[^>]*>/);
    expect(passwordMatch).not.toBeNull();
    if (passwordMatch !== null) {
      expect(passwordMatch[0]).toMatch(/enterkeyhint="go"/);
    }
  });

  it('mobile media query bumps .form-select to 16px to prevent iOS auto-zoom', () => {
    // iOS Safari zooms in on any form control with font-size <16px and
    // doesn't zoom back out after dismissal. The mobile-input rule
    // already covers <input>; this assertion ensures <select> gets the
    // same treatment.
    const mediaBlock = indexHtml.match(
      /@media\s*\(max-width:\s*768px\)\s*\{[\s\S]*?\.form-select\s*\{[\s\S]*?font-size:\s*16px/
    );
    expect(mediaBlock).not.toBeNull();
  });

  it('session-timeout options use consistent Title Case', () => {
    const selectMatch = indexHtml.match(/<select[^>]*id="session-timeout"[\s\S]*?<\/select>/);
    expect(selectMatch).not.toBeNull();
    if (selectMatch !== null) {
      // Default is the only "special" label; the hour-N options should
      // all read "N Hour(s)" with Title Case to match.
      expect(selectMatch[0]).toMatch(/>1 Hour</);
      expect(selectMatch[0]).toMatch(/>4 Hours</);
      expect(selectMatch[0]).toMatch(/>8 Hours</);
      expect(selectMatch[0]).toMatch(/>24 Hours</);
    }
  });
});

describe('index.html — idle-warning dismiss button (#87 G7)', () => {
  it('renders the warning banner with a text span and a dismiss button', () => {
    // The text node is separated from the dismiss button so showIdleWarning
    // can update wording without clobbering the button.
    expect(indexHtml).toMatch(/<span\s+class="idle-warning-text">/);
    expect(indexHtml).toMatch(
      /<button[^>]*class="idle-warning-dismiss"[^>]*id="idle-warning-dismiss"[^>]*aria-label="Dismiss warning"/
    );
  });

  it('declares an `.idle-warning-dismiss` style block', () => {
    expect(indexHtml).toMatch(/\.idle-warning-dismiss\s*\{/);
  });

  it('joins the touch-target-min selector list on mobile', () => {
    // Smoke that the dismiss button picks up the same 44x44 min-target
    // floor as the other panel close/clear buttons. The selector list
    // is comma-separated; locate the rule by anchoring on the var()
    // declaration and checking `.idle-warning-dismiss` appears in the
    // preceding selector list.
    const ruleMatch = indexHtml.match(
      /([^{}]*?\.idle-warning-dismiss[^{}]*?)\{[^{}]*?min-(?:width|height):\s*var\(--touch-target-min\)/
    );
    expect(ruleMatch).not.toBeNull();
  });
});
