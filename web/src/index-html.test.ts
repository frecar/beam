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

describe('index.html — narrow-desktop / tablet status bar (#87 B2 + G1)', () => {
  it('declares the 481-1100px icon-only status-bar media query block', () => {
    // G1 (#94) extended the original 769-1100px B2 fix down to 481px so
    // tablet portrait (iPad ~768px, Pixel-tablet ~800px) gets the same
    // icon-only treatment instead of the entire bar being hidden.
    expect(indexHtml).toMatch(/@media\s*\(min-width:\s*481px\)\s*and\s*\(max-width:\s*1100px\)/);
  });

  it('hides .btn-label inside #status-bar within that block', () => {
    // Anchor on the media query opener so we don't false-positive on
    // unrelated `.btn-label { display: none }` rules elsewhere.
    const block = indexHtml.match(
      /@media\s*\(min-width:\s*481px\)\s*and\s*\(max-width:\s*1100px\)\s*\{([\s\S]*?)\n\s{6}\}/
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

  it('only hides the entire status bar below 480px (G1: phone-only)', () => {
    // G1 (#94) split: tablet (481-768px) keeps the icon-only status
    // bar; phone (<=480px) hides it entirely and the enriched FAB
    // covers every control. The hide rule must live in the 480px
    // block — NOT the 768px block — or tablet regresses.
    expect(indexHtml).toMatch(
      /@media\s*\(max-width:\s*480px\)[\s\S]*?#status-bar\.visible\s*\{\s*display:\s*none/
    );
    // And the 768px block must NOT contain the hide rule any more —
    // a regression here would re-hide on tablet. We allow the rule
    // to appear ONLY inside the 480px block. Strip out the 480px
    // block first, then assert the 768px block contains no
    // `#status-bar.visible { display: none }`.
    const stripped = indexHtml.replace(/@media\s*\(max-width:\s*480px\)\s*\{[\s\S]*?\n\s{6}\}/, '');
    const block768 = stripped.match(/@media\s*\(max-width:\s*768px\)\s*\{([\s\S]*?)\n\s{6}\}/);
    expect(block768).not.toBeNull();
    if (block768 !== null) {
      expect(block768[1]).not.toMatch(/#status-bar\.visible\s*\{\s*display:\s*none/);
    }
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

describe('index.html — mobile FAB enrichment (#87 G1)', () => {
  /*
   * G1 (#94) expanded the phone FAB menu from 4 to 12 actions so the
   * status-bar controls hidden on <=480px viewports remain reachable.
   * These assertions guard the enriched surface against silent
   * regressions during future style/markup sweeps.
   */

  it('renders every enriched FAB action', () => {
    // The audit identifies 9 status-bar controls invisible on phone
    // before this change. The enriched menu MUST host each one (or
    // a select for the layout/scroll-speed pickers).
    const requiredIds = [
      'fab-keyboard',
      'fab-layout-select',
      'fab-scroll-speed-select',
      'fab-upload',
      'fab-download',
      'fab-fullscreen',
      'fab-screenshot',
      'fab-theme',
      'fab-mute',
      'fab-forward-keys',
      'fab-end-session',
      'fab-disconnect',
    ];
    for (const id of requiredIds) {
      expect(indexHtml).toMatch(new RegExp(`id="${id}"`));
    }
  });

  it('every FAB action declares a descriptive aria-label', () => {
    // Match `.fab-action` buttons AND `.fab-action-select` divs.
    const elRegex =
      /<(button|div)[^>]*class="[^"]*\bfab-action(?:-select)?\b[^"]*"[^>]*>[\s\S]*?<\/\1>/g;
    const els = indexHtml.match(elRegex) ?? [];
    expect(els.length).toBeGreaterThanOrEqual(12);
    for (const el of els) {
      // Either the element itself, or its inner <select>, must carry
      // an aria-label.
      expect(el).toMatch(/aria-label="[^"]+"/);
    }
  });

  it('FAB menu container caps height + scrolls when overflowed', () => {
    // The enriched menu may grow beyond a phone's viewport when all
    // 12 rows are visible. The container must scroll its overflow
    // and use dvh so iOS Safari URL-bar collapse doesn't shift the
    // threshold mid-interaction.
    expect(indexHtml).toMatch(
      /#mobile-fab-menu\s*\{[\s\S]*?max-height:\s*min\([\s\S]*?dvh[\s\S]*?\)/
    );
    expect(indexHtml).toMatch(/#mobile-fab-menu\s*\{[\s\S]*?overflow-y:\s*auto/);
  });

  it('phone (<=480px) extends SIP + CHP panels to bottom: 0 (P19 cascade)', () => {
    // Without this, 28px of dead space sits at the bottom of the
    // panels because they reserve room for a now-hidden status bar.
    const phoneBlock = indexHtml.match(/@media\s*\(max-width:\s*480px\)\s*\{([\s\S]*?)\n\s{6}\}/);
    expect(phoneBlock).not.toBeNull();
    if (phoneBlock !== null) {
      // Match either combined or separate rule blocks for the panels.
      expect(phoneBlock[1]).toMatch(
        /#session-info-panel[\s\S]*?#clipboard-history-panel[\s\S]*?bottom:\s*0/
      );
    }
  });

  it('FAB actions meet the WCAG 2.5.5 touch-target floor', () => {
    // PR #93 introduced --touch-target-min: 44px; the enriched FAB
    // rows must honor it so thumb taps land reliably.
    expect(indexHtml).toMatch(/\.fab-action\s*\{[\s\S]*?min-height:\s*var\(--touch-target-min\)/);
    expect(indexHtml).toMatch(
      /\.fab-action-select\s*\{[\s\S]*?min-height:\s*var\(--touch-target-min\)/
    );
  });
});

describe('index.html — FAB polish (#87 G2)', () => {
  /*
   * G2 (#95) layered safe-area handling, inline SVG icons, the
   * Hide-keyboard action, and the 2-tap inline confirm on top of
   * the G1 enriched FAB. These assertions guard the polish against
   * silent regressions.
   */

  it('declares viewport-fit=cover so safe-area-inset env values resolve', () => {
    // Without `viewport-fit=cover` the env(safe-area-inset-*)
    // values return 0 on iOS Safari and the FAB visually overlaps
    // the home indicator on notched iPhones.
    expect(indexHtml).toMatch(/<meta\s+name="viewport"[^>]*viewport-fit=cover/);
  });

  it('FAB position respects safe-area-inset-bottom', () => {
    expect(indexHtml).toMatch(
      /#mobile-fab\s*\{[\s\S]*?bottom:\s*calc\(20px\s*\+\s*env\(safe-area-inset-bottom/
    );
  });

  it('toast container clears the FAB AND the safe-area-inset stack', () => {
    // The toast was previously `bottom: 88px` — fine when FAB sat
    // flush; now that FAB lifts up by safe-area-inset, toasts must
    // add the same inset on top of the FAB height + gap to stay
    // above both the FAB and the home indicator.
    expect(indexHtml).toMatch(
      /#toast-container\s*\{[\s\S]*?bottom:\s*calc\(88px\s*\+\s*env\(safe-area-inset-bottom/
    );
  });

  it('mobile keyboard input is positioned at bottom-left, not off-screen', () => {
    // Before G2, `left: -9999px` caused iOS Safari scroll-jiggle on
    // focus. Anchoring the 1x1 invisible input on-screen at
    // bottom-left removes the scroll-into-view side effect while
    // keeping the input visually absent.
    const inputCss = indexHtml.match(/#mobile-keyboard-input\s*\{[\s\S]*?\n\s{6}\}/);
    expect(inputCss).not.toBeNull();
    if (inputCss !== null) {
      expect(inputCss[0]).not.toMatch(/left:\s*-9999px/);
      expect(inputCss[0]).toMatch(/left:\s*0\b/);
      expect(inputCss[0]).toMatch(/bottom:\s*0\b/);
      expect(inputCss[0]).toMatch(/pointer-events:\s*none/);
      expect(inputCss[0]).toMatch(/font-size:\s*16px/); // iOS anti-zoom
    }
  });

  it('renders the "Hide keyboard" FAB action', () => {
    expect(indexHtml).toMatch(/id="fab-keyboard-hide"/);
    expect(indexHtml).toMatch(
      /<button[^>]*id="fab-keyboard-hide"[^>]*aria-label="Hide on-screen keyboard"/
    );
  });

  it('declares a .confirming state for destructive FAB actions', () => {
    // 2-tap inline confirm needs a visible state. The `.confirming`
    // class flips the row into a red-outlined state so the user
    // sees what's about to happen on the second tap.
    expect(indexHtml).toMatch(/\.fab-action\.confirming\s*\{[\s\S]*?outline[\s\S]*?#ff6b6b/);
  });

  it('FAB action SVGs are constrained to consistent dimensions', () => {
    // Hydrated icons should size uniformly across rows.
    expect(indexHtml).toMatch(/\.fab-action\s+svg\s*\{[\s\S]*?width:\s*18px/);
    expect(indexHtml).toMatch(/\.fab-action\s+svg\s*\{[\s\S]*?height:\s*18px/);
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
