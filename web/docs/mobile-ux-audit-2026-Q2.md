# Mobile / Touch UX Baseline Audit — Beam Web Client (2026-Q2)

> Audit pass — no fixes in this branch. Each finding is filed (or to be filed) as a follow-up sub-issue under #87.
>
> Scope: every non-canvas UI surface in `web/`. The streaming canvas itself (`#remote-canvas` + `webcodecs-renderer.ts`) is touched only where its touch handling interacts with surrounding chrome (FAB, status bar, drawers).
>
> Methodology: static review of `web/index.html`, `web/src/*.ts`, the touch handlers in `input.ts`, and the responsive CSS at the bottom of `index.html`. Target viewports: 360x640 (small Android), 390x844 (iPhone 14), 768x1024 (iPad portrait), 1024x768 (iPad landscape). Reference baseline: WCAG 2.5.5 (target size minimum 44x44 CSS pixels), iOS HIG (44x44 pt), Material (48x48 dp).
>
> Definitions:
>
> - **Blocker** — the surface is unusable or actively misleading on a phone. Must be fixed before claiming mobile support.
> - **Pain** — the surface works but fights the user (small targets, awkward scroll, overlap, jank). Should be fixed.
> - **Minor** — cosmetic / nice-to-have. Defer freely.

## Surface inventory

| # | Surface | File(s) | Verdict | Highest finding |
|---|---|---|---|---|
| 1 | Viewport meta | `index.html:5` | Pass | — |
| 2 | Login view (`#login-view`) | `index.html:1633-1698`, `login.ts`, `ui-state.ts` | Pain | Several |
| 3 | Login loading / cancel | `index.html:1692-1696` | Pass | Minor |
| 4 | Login error / rate-limit banner | `index.html:1677`, `login.ts:37-66` | Pass | Minor |
| 5 | Status bar (`#status-bar`) | `index.html:1850-1903`, `ui.ts` | Blocker | Hidden entirely on `<=768px`; 9-button toolbar nuked on tablet portrait |
| 6 | Mobile FAB (`#mobile-fab`) | `index.html:1839-1848`, `main.ts:1282-1332` | Pain | Several |
| 7 | Session-info panel (`#session-info-panel`) | `index.html:1740-1771` | Pain | Several |
| 8 | Clipboard-history panel (`#clipboard-history-panel`) | `index.html:1774-1785` | Pain | Several |
| 9 | Admin sessions panel (`#admin-panel-overlay`) | `index.html:1788-1812` | Pain | Several |
| 10 | Help overlay (`#help-overlay`) | `index.html:1815-1828` | Pain | Useless on touch (no F-keys) |
| 11 | Reconnect overlay (`#reconnect-overlay`) | `index.html:1712-1730` | Pass | Minor |
| 12 | File-drop overlay (`#file-drop-overlay`) | `index.html:1707-1709` | Pain | Touch devices have no drop affordance |
| 13 | Toast container (`#toast-container`) | `index.html:1834`, `ui.ts:45-80` | Pass | Minor |
| 14 | Idle warning banner (`#idle-warning`) | `index.html:1734`, `settings.ts:177-188` | Pain | Top-pinned banner has no dismiss |
| 15 | Performance overlay (`#perf-overlay`) | `index.html:1737` | Pass | Touch-only users have no F9 |
| 16 | Hidden virtual-keyboard input | `index.html:1837`, `main.ts:1335-1371` | Pain | Several |
| 17 | Streaming canvas gesture routing | `input.ts:758-841` | Pain | No two-finger pan/scroll, no pinch |
| 18 | `window.prompt()` for download path | `main.ts:1275` | Blocker | Native prompt + path entry, no file browser |

---

## Blockers

### B1 — Status bar invisible on phones, 9 controls become unreachable

**File**: `index.html:1566-1569` (CSS), `index.html:1850-1903` (DOM)

The responsive block at `index.html:1566-1569` does:

```css
@media (max-width: 768px) {
  #status-bar.visible {
    display: none;
  }
}
```

This hides the entire status bar — and with it 13 controls: connection dot, FPS / Dec / loss stats, keyboard-layout selector, scroll-speed selector, Upload, Download, Capture (forward keys), Mute, Fullscreen, Theme, Disconnect, End Session, version. The mobile FAB (`#mobile-fab`) replaces this — but it only contains **four** actions: Keyboard, Fullscreen, Screenshot, Disconnect (`index.html:1842-1845`).

That leaves the following functionality with **no mobile entry point at all**:

| Missing on mobile | Live in status bar at | Impact |
|---|---|---|
| Keyboard layout (US/GB/NO/SE/DK/DE/FR/ES/FI/IT/PT/NL/PL/RU/JP) | `index.html:1867-1883` | Cannot pick a non-US layout from phone. |
| Scroll speed (0.5x / 1x / 2x / 3x) | `index.html:1885-1890` | No way to dial scroll for touch-pad mode. |
| Upload file | `index.html:1892` + `<input type=file>` at `1893` | Cannot upload from phone gallery / cloud picker. |
| Download file (`window.prompt`) | `index.html:1894`, `main.ts:1273-1280` | Path-prompt download is mobile-hostile anyway, see B3. |
| Capture browser keys toggle | `index.html:1895` | Less critical on mobile (no browser shortcuts to capture), but invisible. |
| Mute / unmute | `index.html:1896` | Cannot toggle audio. |
| Theme toggle | `index.html:1898` | Cannot override system preference. |
| End Session (vs. Disconnect) | `index.html:1900` | Cannot end-and-release. FAB only has Disconnect. |
| Connection status dot + text | `index.html:1853-1854` | No live indicator of connecting/connected/error during streaming. |
| Bandwidth indicator | `index.html:1855` | No bitrate / total-bytes feedback. |
| FPS / Decode / loss stats | `index.html:1858-1864` | No quick stats glance — must open SIP via F10 (no F-key on phone). |

Tablet portrait (768px or below) gets the same treatment, which is a larger regression: a tablet has the screen real estate but is bucketed identically with a 360px phone.

**Screenshot description**: iPhone 14 portrait (390x844) with an active session. Bottom of screen shows the streamed canvas all the way to the home indicator. FAB plus-icon (56x56) hovers at bottom-right. No status indicator visible. Compare to desktop view: dense 28px bar across the bottom with 13 controls.

**Proposed fix** (defer to sub-issue): replace the all-or-nothing CSS rule with one of:

1. Keep the status bar visible on tablet (`@media (max-width: 480px)` only hides it — let `481-768px` show a tablet variant).
2. Expand the FAB menu to expose at least: keyboard-layout, scroll-speed, upload, mute, theme, end-session.
3. Add a phone-tailored bottom drawer (separate from SIP) holding the layout/scroll selects, since `<select>` elements are mobile-friendly.

### B2 — Status bar at 768px (tablet portrait): horizontal overflow before hide kicks in

**File**: `index.html:600-604` (`.status-right`), `index.html:1850-1903`

Independent of B1: even on devices >768px (e.g. an iPad in landscape at 1024px width, or a small laptop at 800px), the `.status-right` block runs:

- 4-stat group (FPS | Dec | loss + hidden RTT)
- vertical separator
- keyboard-layout `<select>` (US wide enough; `JP`, `RU`, etc.)
- vertical separator
- scroll-speed `<select>` ("Scroll 1x")
- vertical separator
- 7 buttons each with a 14x14 SVG icon + `<span class="btn-label">` label: Upload, Download, Capture, Mute, Fullscreen, Theme, Disconnect, End Session

Quick measurement of the labels alone (no padding):

| Button | Label | Approximate width including 14px icon + 4px gap + 8px horizontal padding each side |
|---|---|---|
| Upload | "Upload" | ~80px |
| Download | "Download" | ~96px |
| Capture | "Capture" | ~88px |
| Mute / Unmute | "Unmute" | ~88px |
| Fullscreen | "Fullscreen" | ~104px |
| Theme | "Light" / "Dark" | ~70px |
| Disconnect | "Disconnect" | ~104px |
| End Session | "End Session" | ~112px |

7+ buttons average ~95px each = ~665px before separators and selects. Stats group + 2 selects = ~250px. Total ~915px just for `.status-right`, plus status-left dot + text ~150px. **At 800-1000px viewports the bar overflows horizontally** with no `overflow: hidden` on `#status-bar` and no flex-wrap — meaning controls get clipped or push the layout.

**Screenshot description**: iPad portrait 768x1024 viewport showing the status bar pushed beyond viewport-right, "End Session" button missing or clipped.

**Proposed fix** (defer): media-query below ~1100px to hide button labels (icon-only with `aria-label`), or `flex-wrap: wrap` with row spacing.

### B3 — `window.prompt()` for download path is unusable on touch

**File**: `main.ts:1273-1280`

```ts
btnDownload.addEventListener('click', () => {
  const path = window.prompt('Enter file path on remote desktop (relative to home or absolute):');
  if (path && connection) {
    ui?.showNotification(`Requesting download: ${path}`, 'info', 2000);
    connection.sendInput({ t: 'fdr', path } as InputEvent);
  }
});
```

Two compounding problems on touch:

1. `window.prompt()` is one-line text input. On iOS Safari it's a system-level modal that auto-zooms the page and offers no completion / autocomplete. Asking the user to type an absolute path on the remote filesystem from a phone keyboard is hostile.
2. The control is in the (currently mobile-hidden) status bar — so this only fires on desktop today, but if B1 surfaces it on mobile (recommended), the prompt becomes the user's first touch experience.

**Screenshot description**: not applicable — no surface on mobile currently. But once exposed, would be the system iOS prompt dialog with a long path placeholder.

**Proposed fix** (defer): replace prompt with a remote file-picker UI (server-side `/api/files?path=...` listing), or — minimum — only expose Download on platforms where typing a path makes sense (desktop only).

**Resolution (G8 #100)** — **chosen: in-app path-entry modal** (a refinement of the two options above). Rather than build a server-side `/api/files` listing endpoint (Option 1 — new attack surface: path-traversal / symlink-escape hardening, deferred) or hide Download on touch (Option 2 — removes a capability), the `window.prompt()` call is replaced with a styled in-app modal (`#download-overlay`) that keeps the remote-path-entry semantics. The modal mirrors the existing `#help-overlay` pattern: a 16px-font text input (defeats iOS focus auto-zoom — the same fix as P1), Cancel/Download actions sized to the WCAG touch-target floor (`--touch-target-min`), an explicit close X (Esc is captured by the remote session's mobile-keyboard input on touch), backdrop-tap-to-close, and focus restoration on close. The chosen path still flows through the unchanged `connection.sendInput({ t: 'fdr', path })` call — this is the **remote source path** the agent reads, NOT a local save destination (so the File System Access API `showSaveFilePicker()` is deliberately not used; it solves a different problem). Logic extracted to `web/src/download-prompt.ts` with regression tests in `web/src/download-prompt.test.ts`. A full server-side folder browser (Option 1) remains a worthwhile future enhancement but is out of scope here.

---

## Pain points

### P1 — Login: `<select>` for session timeout uses default font-size on mobile, iOS auto-zooms

**File**: `index.html:240-262`, `1514-1546` (responsive)

The `.form-select` base style (`index.html:240`) sets `font-size: 14px`. The mobile media query at `index.html:1531-1535` bumps `.form-group input` to `font-size: 16px` to prevent iOS auto-zoom on focus — but doesn't extend the same rule to `.form-select`. iOS Safari zooms in on any form control with font-size <16px, then doesn't zoom back out.

**Screenshot description**: iPhone Safari, "Session timeout" select tapped. Viewport zooms 1.3x to fit the (14px) text, top of login card scrolls out of view, no auto-zoom-back-out after select dismissed.

**Proposed fix** (defer): add `.form-select { font-size: 16px; }` to the 768px media query.

### P2 — Login: password "Show" toggle is below 44px target

**File**: `index.html:272-289`

```css
.password-toggle {
  position: absolute;
  right: 8px;
  top: 50%;
  transform: translateY(-50%);
  ...
  padding: 2px 4px;
}
```

The "Show" label is ~28x18px including the `font-size: 12px` + 2/4px padding. WCAG 2.5.5 min target is 44x44 CSS px. There is no mobile override raising the padding or `min-height` for this button. Inside a `position: absolute` inside `.password-wrapper` it would need `inset` adjustment to grow without breaking the form layout.

**Screenshot description**: iPhone portrait, password field focused. The tiny "Show" text sits inside the password input's right edge — touch hit-test fights with the input's focus.

**Proposed fix** (defer): increase the touch target to >=44px via `padding: 12px 16px` + adjusting input `padding-right` accordingly, or replace the text with a 44x44 icon button.

### P3 — Login: timeout `<select>` last in tab order, no `enterkeyhint`

**File**: `index.html:1639-1675`

Username and password inputs lack `enterkeyhint="next"` / `enterkeyhint="go"`, so iOS Safari shows the generic "return" key instead of "Next" / "Go". Form submit-on-Enter works (it's `<form id="login-form">`) but the hint cues the user to move on rather than dismiss.

**Screenshot description**: iPhone Safari, username field focused. Soft keyboard shows generic "return" key bottom-right.

**Proposed fix** (defer): `enterkeyhint="next"` on username, `enterkeyhint="go"` on password.

### P4 — FAB toggle never anchored to safe-area-inset on iPhone

**File**: `index.html:1296-1325`

```css
#mobile-fab {
  position: fixed;
  bottom: 20px;
  right: 20px;
  ...
}
```

On iPhone with home-indicator, `bottom: 20px` clears the indicator by about 14px — barely. On older iPhones with no home indicator (SE-class 4.7"), 20px is fine. On Android Chrome with on-screen navigation bar, 20px sits underneath the system nav and gets occluded during edge gestures.

There is no `env(safe-area-inset-bottom)` adjustment and no `viewport-fit=cover` in the viewport meta — so the page doesn't even enter safe-area-respecting layout on notched devices.

**Screenshot description**: iPhone 14 Pro screenshot, FAB plus-icon. The 56x56 button sits with its bottom edge 20px above the home indicator — the indicator visually overlaps the bottom 6-8px of the button.

**Proposed fix** (defer): `viewport-fit=cover` in the meta; `bottom: calc(20px + env(safe-area-inset-bottom))` on `#mobile-fab`. Same for `#status-bar` once it's mobile-visible.

### P5 — FAB menu actions: no visible icons + Disconnect lacks confirm

**File**: `index.html:1841-1847`, `main.ts:1322-1325`

```html
<button class="fab-action" id="fab-keyboard">Keyboard</button>
<button class="fab-action" id="fab-fullscreen">Fullscreen</button>
<button class="fab-action" id="fab-screenshot">Screenshot</button>
<button class="fab-action fab-action-disconnect" id="fab-disconnect">Disconnect</button>
```

Two issues:

1. Text-only actions in a FAB menu — every other FAB-style UI in iOS/Android uses icon + label. Pure text is unusual and increases vertical real estate.
2. `fab-disconnect` immediately calls `handleDisconnect()` with no confirmation. On a 56x56 FAB next to a streaming canvas with frequent thumb travel, an accidental tap kills the session. (Compare: the status-bar `Disconnect` and `End Session` are physically far from the fullscreen-canvas centroid.)

**Screenshot description**: FAB menu expanded — four white pill buttons cascading up from the plus FAB, last one (Disconnect) in red text.

**Proposed fix** (defer): add inline SVG icons matching the status-bar icons; wrap Disconnect in a 2-tap confirm or "swipe-to-disconnect" gesture.

### P6 — FAB-keyboard: focus dance with hidden input causes auto-scroll jank

**File**: `index.html:1370-1379`, `main.ts:1307-1310`

```css
#mobile-keyboard-input {
  position: fixed;
  left: -9999px;
  top: 0;
  width: 1px;
  height: 1px;
  opacity: 0;
  font-size: 16px;
}
```

Focusing an off-screen-fixed input opens the soft keyboard, but iOS Safari sometimes scrolls the page to "bring the focused element into view" — which it can't (it's at -9999px) — resulting in a janky scroll-jiggle. Android Chrome handles it better but still warps the layout viewport.

Worse: `font-size: 16px` prevents auto-zoom on focus, but the comment in the source claims that's the reason for it. On Android the keyboard appears beneath the FAB — the FAB still receives taps, so the Keyboard action does work — but the user has no visible signal that the keyboard came from "this" input.

**Screenshot description**: iPhone Safari, user taps FAB > Keyboard. Soft keyboard slides up. Page may scroll left ~20-50px attempting to focus the off-screen input. No caret indicator anywhere on the visible page.

**Proposed fix** (defer): position the hidden input at `bottom: 0; left: 0;` with `opacity: 0; pointer-events: none;` instead of `left: -9999px`. iOS will then scroll naturally to it (it's already in view). Alternatively: open the input as a visible thin strip above the soft keyboard for explicit feedback.

### P7 — FAB-keyboard: no dismiss path other than tapping outside the page input field

**File**: `main.ts:1335-1371`

The hidden input only blurs on `Escape` keypress (line 1363). There's no FAB button to "Hide Keyboard" — users have to tap the OS-level "Dismiss keyboard" gesture (varies per Android keyboard) or hunt for the dismiss button on iOS keyboards. On iPad the soft-keyboard dismiss is bottom-right inside the keyboard, but on iPhone there's none.

Worst case: user opens keyboard, can't find dismiss, taps somewhere else on the canvas — that triggers a remote click + keyboard remains open.

**Screenshot description**: iPhone Safari, soft keyboard shown over bottom half of streamed canvas. User taps in upper half hoping to dismiss keyboard. Tap is interpreted as remote click; keyboard stays.

**Proposed fix** (defer): add a "Hide keyboard" action that fires `mobileKeyboardInput.blur()`. Either as a second FAB action or a small floating chip above the soft keyboard.

### P8 — Session-info panel: 320px width is wider than 320px viewports

**File**: `index.html:856-869`, `1587-1591` (responsive)

Base width is 320px (line 858). Responsive at <=768px sets it to 100% (line 1589). So at 320-360px viewports the responsive rule kicks in correctly — good. But the right-slide-in transform is `translateX(100%)` which, on the responsive override, works fine.

The actual pain: SIP rows use `display: flex; justify-content: space-between` with `font-family: "SF Mono"...` for `.sip-value` and `word-break: break-all`. On narrow viewports a long session ID (UUID, ~36 chars) word-breaks into multiple lines that push the label off — there's no fixed proportion. Also the `aside` panel is full-height minus 28px status bar (line 857: `bottom: 28px`) — but the status bar is hidden on mobile (B1), so 28px of dead space is reserved at the bottom of the panel for nothing.

**Screenshot description**: iPhone portrait with SIP open: rows show "Session ID" on one line, then a long UUID wrapping across 3 visual lines. Below SIP entries, 28px of empty bottom margin above the FAB.

**Proposed fix** (defer): when status bar is hidden, also set `#session-info-panel { bottom: 0; }` in the same media query. Tighten the row layout: drop `font-family` to use the sans inherit on mobile so the UUID flows better.

### P9 — Session-info panel: no swipe-to-close; "X" close button is small

**File**: `index.html:892-908`, `1740-1771` (DOM)

```css
.sip-close {
  background: none; border: none;
  color: var(--text-tertiary);
  font-size: 18px;
  cursor: pointer;
  padding: 0 4px;
  line-height: 1;
}
```

Close button is ~14x18px (the `×` glyph + 4px padding) — well below 44x44. Common pattern: swipe-right-to-close on right-side panels. Not implemented.

**Screenshot description**: SIP open on iPhone, "Session Info" header with a tiny `×` to the right. Thumb misses 30% of the time.

**Proposed fix** (defer): bump close button to 44x44 min target via `padding: 12px 16px`; add `touchstart`/`touchmove` swipe-right handler on the panel to dismiss.

### P10 — Clipboard-history panel: same as P8 + P9, plus copy buttons are 10px font

**File**: `index.html:946-1079`, `1593-1597` (responsive)

Clipboard history mirrors SIP's layout (320px wide, slides in from right). It has the same:

- 28px dead-bottom on mobile (B1 cascade — `bottom: 28px` at line 951).
- Tiny close button (~14x18px).
- Tiny clear button (`.chp-clear` is ~24x20px including padding).
- Per-entry copy button (`.chp-copy`) at `font-size: 10px`, `padding: 1px 6px` — effective hit area ~18x14px. Hostile on touch.

There's no touch handler for entries themselves — the only way to "use" a clipboard entry is to tap the 18x14 copy button. The `:hover` rule (`.chp-entry:hover { background: ...}`) has no `:active` equivalent so touch users get no visual feedback when finger lands.

**Screenshot description**: iPhone portrait, clipboard panel open showing 4 entries. Each entry has a 5-character text preview, "↘ Received 2m ago" meta, and a tiny "Copy" button right-aligned. Tap target for copy is ~18x14px.

**Proposed fix** (defer): make the entire `.chp-entry` row tappable (full-row copy on touch), with the 10px copy button kept as a visual affordance; add `:active` background; restore the bottom inset to 0 on mobile.

### P11 — Admin panel: 560px min-width is wider than any phone

**File**: `index.html:1394-1405`, `1504-1511` (responsive)

```css
.admin-panel {
  ...
  min-width: 560px;
  max-width: 720px;
  width: 90%;
  ...
}

@media (max-width: 768px) {
  .admin-panel {
    min-width: auto;
    width: calc(100% - 32px);
    max-height: 90vh;
  }
  .admin-table { font-size: 11px; }
}
```

The mobile override correctly relaxes `min-width: auto` and shrinks to viewport minus 32px. Good. But the table inside (`index.html:1796-1809`) has 6 columns: Session, User, Display, Created, Idle, [Terminate button]. Even at `font-size: 11px` on a 360px viewport, fitting 6 cols of monospace data is a horizontal-scroll situation. There's no `overflow-x: auto` on the `.admin-panel-body` — only `overflow-y: auto`. Long session IDs or usernames push the table sideways and the surrounding card grows / clips.

**Screenshot description**: iPhone portrait, admin panel open. The 6-col table either (a) shrinks columns to unreadable widths with text-overflow ellipsis cutting session IDs to "abc12...", or (b) overflows horizontally with no scroll affordance.

**Proposed fix** (defer): on mobile, convert the table to a stacked-card list (one card per session), each card with Session+User on row 1, Display+Created+Idle on row 2, Terminate full-width below. Or `overflow-x: auto` on `.admin-panel-body` with a visible scroll hint.

### P12 — Admin panel: Terminate button is 26x16px (way below target)

**File**: `index.html:1481-1502`

```css
.admin-terminate-btn {
  padding: 3px 10px;
  ...
  font-size: 11px;
}
```

Terminate is a destructive action — and the touch target is roughly 26x16px (text "Terminate" + 3/10px padding). On a list of 5+ sessions, accidentally terminating the wrong one is plausible.

**Screenshot description**: Admin panel on iPad portrait, table shows 5 active sessions, last column has tiny red "Terminate" links. Accidental thumb taps are likely.

**Proposed fix** (defer): bump to >=44x44 with confirm dialog ("Terminate session for `<username>`?").

### P13 — Help overlay: lists F-keys with no mobile equivalent

**File**: `index.html:1815-1828`

```html
<div class="help-card">
  <h2>Keyboard Shortcuts</h2>
  <div><kbd>F1</kbd> Toggle this help</div>
  ...
</div>
```

Phones have no F-keys. The help overlay is the entire shortcut reference, accessible only by pressing F1 — which mobile users can't do. So this surface is effectively dead code on touch. (No FAB entry, no other discoverability.)

**Screenshot description**: not viewable on mobile.

**Proposed fix** (defer): on touch devices, replace `.help-card` content with the equivalent FAB-mapped actions OR hide help entirely (no help button anywhere on the FAB). At minimum, surface help via the FAB menu.

### P14 — File-drop overlay: drop is desktop-only; no touch upload affordance once status bar hidden

**File**: `index.html:1707-1709`, `1235-1259` (CSS), `1892-1893` (upload button)

File upload has two affordances on desktop:

1. The `#file-drop-overlay` div that appears on `dragover` — useless on touch (no drag-and-drop file API on phones, only on iPad with hardware kbd).
2. The `#btn-upload` button + `<input type="file" multiple>` in the status bar — the status bar is hidden on mobile (B1), so this entry point is gone too.

Net result: no way to upload files from a phone.

**Screenshot description**: iPhone session, user wants to upload a photo from camera roll. There is no UI for it.

**Proposed fix** (defer): add an Upload FAB action that triggers the `<input type=file>` click via JS. iOS Safari will offer "Photo Library" / "Take Photo" / "Choose File" picker.

### P15 — Idle warning banner: no dismiss, opaque to skip on mobile

**File**: `index.html:1734`, `settings.ts:177-188`

The `#idle-warning` is `position: fixed; top: 0;` — slides down from the top and stays until activity is detected. On mobile with FAB at bottom and canvas filling the middle, the banner overlays the top of the canvas content. There's no "Dismiss" button — only mouse/key activity hides it. On touch, `touchstart`/`touchmove` count as activity (per `main.ts:1374-1377`), so any tap dismisses it — but a user reading the message statically would have to tap the canvas just to acknowledge.

**Screenshot description**: iPhone, idle warning banner pinned at top reading "Session expires in 1m 23s. Move mouse or press a key to stay connected." Lower 95% of viewport is the streamed canvas.

**Proposed fix** (defer): add an "X" dismiss + reword to "tap anywhere" instead of "Move mouse or press a key" on touch devices.

### P16 — Performance overlay: F9 only; no FAB toggle; not useful on mobile

**File**: `index.html:1737`, `index.html:832-850` (CSS)

`#perf-overlay` is positioned `top: 8px; left: 8px;` — would overlap a phone's notch / status bar / dynamic island. Toggled via F9 only. Without F-keys, mobile users can't activate it. Not a regression (perf overlay is power-user only) — but worth surfacing through the FAB if mobile streaming is intended to be more than a "view-only" mode.

**Screenshot description**: not applicable on mobile.

**Proposed fix** (defer): no immediate action; revisit if/when mobile becomes a primary platform.

### P17 — Streaming canvas: no two-finger pan, no pinch, no two-finger scroll

**File**: `input.ts:758-841`

Touch handlers in `input.ts` short-circuit on `e.touches.length !== 1`:

```ts
private handleTouchStart(e: TouchEvent): void {
  e.preventDefault();
  if (e.touches.length !== 1) {
    this.cancelLongPress();
    return;
  }
  ...
}
```

This is correct for sending mouse-position events to the remote, but it means:

- **Pinch-zoom is consumed** (`e.preventDefault()` on multi-touch starts) but **not routed anywhere** — the user expects the canvas to "zoom in on the remote desktop" or "zoom the page itself". Neither happens. (Page zoom is already disabled by `maximum-scale=1, user-scalable=no` in the viewport meta — correctly, to avoid fighting the remote — but the alternative is empty.)
- **Two-finger scroll** to send a scroll event to the remote does not exist. The status bar has a "Scroll speed" select that's only meaningful for mouse-wheel events; touch never generates one.
- **Two-finger pan** to drag the remote viewport (e.g. when remote resolution > local) doesn't exist either.

Note: the issue body explicitly lists "Streaming canvas itself" under the audit scope ("touch-pan/pinch behaviour vs the locked viewport, gesture routing for remote input").

**Screenshot description**: iPhone Safari, an active session showing a desktop with a small terminal. User pinch-zooms; nothing happens. Two-finger drag; nothing happens. Single-finger drag moves the remote mouse but doesn't pan the view.

**Proposed fix** (defer, design-heavy): introduce a touch interaction mode (toggle in FAB): "Pointer" (current) / "Scroll & zoom" (two-finger = scroll, pinch = zoom client-side via canvas transform). This is the cleanest way to give touch users access to scroll without changing the protocol.

### P18 — Toast container: full-width on mobile but stacks behind FAB

**File**: `index.html:506-515`, `1577-1581` (responsive)

```css
@media (max-width: 768px) {
  #toast-container {
    bottom: 88px;
    right: 12px;
    left: 12px;
  }
  .toast { max-width: 100%; }
}
```

The mobile override puts toasts above the FAB (bottom: 88px = 56 FAB height + 20 FAB bottom + 12 gap, roughly). But: 88px is hardcoded — if the FAB grows or moves with safe-area-inset (P4), toasts overlap. Also, toasts are right-anchored on desktop (`right: 16px`) — on mobile they stretch full width edge-to-edge, which is fine, but the close `×` button at `font-size: 16px` is still ~14x14px — below 44px target.

**Screenshot description**: iPhone portrait with a "Upload complete" toast visible, full-width above the FAB. Toast close-X on the right edge is tiny.

**Proposed fix** (defer): bump toast close button to 44x44 target; switch hardcoded 88px to `calc(var(--fab-height, 56px) + 32px)`.

### P19 — Status bar SIP/CHP panels reserve 28px when status bar is hidden (cascade of B1)

**File**: `index.html:856-862` and `index.html:947-961`

Both SIP and CHP have `bottom: 28px;` (above the status bar). On mobile the status bar is hidden — so 28px of empty space at the bottom of the panel is wasted. Same pattern repeated.

(See P8, P10 — flagged here as a single root-cause cascade.)

**Proposed fix** (defer): single media-query rule that zeros `bottom` on both panels when the status bar is mobile-hidden.

---

## Minor findings

### M1 — `viewport-fit=cover` missing from viewport meta

**File**: `index.html:5`

Current:

```html
<meta name="viewport" content="width=device-width, initial-scale=1, maximum-scale=1, user-scalable=no" />
```

Missing `viewport-fit=cover`. Without it, the page does NOT extend under the notch / dynamic island / home indicator — the browser maintains a "safe" layout viewport. That's actually fine for a remote-desktop app today (canvas filling the safe region is what we want), but it prevents `env(safe-area-inset-*)` from working (P4 references this). Decision needed: do we want the canvas to extend under safe-area regions (richer) or not (simpler)?

**Proposed fix** (defer): if/when P4 is fixed, also set `viewport-fit=cover` and apply safe-area-inset to the FAB + status bar.

### M2 — `autocapitalize="off"` on mobile-keyboard-input but no `autocapitalize` on login username

**File**: `index.html:1641-1649` (username) vs `index.html:1837` (mobile-keyboard-input)

The hidden virtual keyboard input correctly sets `autocapitalize="off" autocorrect="off" spellcheck="false"` — preventing iOS from auto-capitalizing the first character of forwarded text. The login username input has `autocomplete="username"` but no `autocapitalize="none"`. So iOS will auto-capitalize "fredrik" into "Fredrik" on phone keyboards (the user has to manually shift-lower it).

**Screenshot description**: iPhone Safari, login screen, soft keyboard up, typing "f" — iOS shows shift-on by default so the field receives "F".

**Proposed fix** (trivial): add `autocapitalize="none"` to the username input.

### M3 — Login: timeout `<option>` values use uppercase like "Default" vs "1 hour"; inconsistent case

**File**: `index.html:1667-1673`

```html
<option value="">Default</option>
<option value="3600">1 hour</option>
```

Aesthetic only — "Default" capitalized vs "1 hour" sentence case. Doesn't affect UX, just looks odd in a dropdown.

**Proposed fix** (trivial): unify to either "Default" + "1 Hour" or "default" + "1 hour".

### M4 — Skip link `top: -100%` is unusual; standard pattern is `top: -40px` or `clip-path`

**File**: `index.html:144-162`

The skip-link uses `top: -100%` (line 147) to hide it off-screen. This works on most desktop browsers but `100%` of a parent that's `position: absolute` and has no defined height resolves to `0` — meaning the skip link may not actually be hidden in some edge cases. (Empirically it does hide because the link itself is absolute-positioned in the body.)

**Proposed fix** (trivial): change to `top: -40px` (standard) or `clip: rect(0,0,0,0); clip-path: inset(50%); height: 1px; width: 1px;` (modern accessible-hidden pattern). Defer.

### M5 — `<kbd>` keyboard shortcuts visible on mobile inside `.login-shortcuts` are hidden via `display: none` — correctly

**File**: `index.html:1548-1555`

```css
@media (max-width: 768px) {
  .login-shortcuts { display: none; }
  .login-footer-hint { display: none; }
}
```

Verified — keyboard shortcut hints are correctly hidden on mobile. Good. Logging here as a "pass" to prevent re-flagging later.

### M6 — Reconnect overlay is mobile-friendly

**File**: `index.html:1124-1233`, `1599-1609` (responsive)

```css
@media (max-width: 768px) {
  .reconnect-card {
    width: calc(100% - 32px);
    max-width: 300px;
  }
  .reconnect-btn-primary,
  .reconnect-btn-secondary {
    min-height: 48px;
    font-size: 16px;
  }
}
```

Both buttons get `min-height: 48px` and `font-size: 16px` on mobile. Touch targets are sufficient. Card resizes correctly. Verdict: Pass.

---

## Cross-cutting observations

1. **Status-bar-hidden cascade is the root cause of many findings**. B1 + P8 + P10 + P14 + P19 all stem from `#status-bar.visible { display: none; }`. Replacing that rule with a tablet-vs-phone split would resolve five findings at once.
2. **44px touch target is violated in ~8 places**: password "Show" toggle, FAB Disconnect (no confirm not a target issue but adjacent), SIP close, CHP close, CHP clear, CHP per-entry copy, admin terminate, toast close. A design-system-class follow-up (which #87 explicitly defers) should set a single `--touch-target-min: 44px` and audit every interactive element.
3. **`<select>` font-size inconsistency**: only `.form-group input` gets the 16px bump on mobile; `.form-select` and other inputs (search? none currently) don't. Worth a single CSS rule.
4. **No safe-area-inset usage**: notched / home-indicator devices receive layouts that visually clip into iOS chrome.
5. **No touch-mode toggle**: P17 is the highest-leverage design decision — currently the canvas eats multi-touch with no fallback. A first-class touch-mode switcher (Pointer / Scroll-and-zoom) would unblock real mobile use.

---

## Suggested sub-issue groupings (for follow-up PRs, not in this branch)

| Group | Findings | Rough effort | Status |
|---|---|---|---|
| G1: Restore mobile controls without hiding status bar | B1, B2, P19 | M | B2 shipped (PR #91); B1+P19 open |
| G2: FAB enrichment (icons, missing actions, safe-area, dismiss) | P4, P5, P6, P7, P14 | M | open |
| G3: Touch-target hygiene pass (44px audit) | P2, P9, P10, P12, P18 | S | shipped — `--touch-target-min: 44px` CSS variable + per-control mobile overrides |
| G4: Login form polish (form-select zoom, enterkeyhint, autocapitalize) | P1, P3, M2, M3 | S | open |
| G5: Panels: SIP, CHP, Admin mobile layouts | P8, P10, P11 (admin) | M | open |
| G6: Touch-mode for canvas (pinch / two-finger scroll) | P17 | L | open |
| G7: Idle warning dismiss + touch wording | P15 | S | open |
| G8: Replace `window.prompt()` download with picker | B3 | M | shipped — in-app `#download-overlay` path-entry modal (#100); replaces `window.prompt()`, 16px input (no iOS zoom), keeps `{ t: 'fdr', path }` remote-path semantics. Server-side folder browser deferred. |
| G9: Help overlay surfaced through FAB | P13, P16 | S | open |

Each group should be its own GitHub sub-issue with `## Relationships → Parent: #87` via the native sub-issue API, severity calibrated to the highest finding in the group (G1 + G6 + G8 = blocker -> severity:high; rest medium/low).

---

## Sign-off checklist (manual, post-fix)

Per the acceptance criteria on #87:

- [ ] iPhone Safari (latest iOS): walk login -> session -> SIP / CHP / admin -> disconnect.
- [ ] Android Chrome (latest): same path.
- [ ] iPad portrait: validate tablet layout for the status bar (post-G1 fix).
- [ ] iPad landscape: validate no regressions in desktop-class layout.
- [ ] Manual verification of safe-area-inset on iPhone with home indicator.
