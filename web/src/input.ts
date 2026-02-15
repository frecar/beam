import type { InputEvent } from "./connection";
import { keyCodeToEvdev } from "./keymap";
import { isBrowserShortcut, isMac } from "./platform";
import { roundToEven, isSignificantResize } from "./resize";

// Re-export for external use (tests, etc.)
export { roundToEven, isSignificantResize } from "./resize";

/** Evdev code for Left Ctrl, used when remapping Mac Cmd shortcuts */
const EVDEV_LEFT_CTRL = 29;

/**
 * Keyboard layout signatures: map physical key codes to the characters
 * they produce on each layout. Used with the Keyboard Layout Map API
 * (Chrome/Edge) to detect the actual OS keyboard layout.
 */
const LAYOUT_SIGNATURES: Record<string, Record<string, string>> = {
  no:  { BracketLeft: "\u00e5", Semicolon: "\u00f8", Quote: "\u00e6" },  // å ø æ
  se:  { BracketLeft: "\u00e5", Semicolon: "\u00f6", Quote: "\u00e4" },  // å ö ä
  dk:  { BracketLeft: "\u00e5", Semicolon: "\u00e6", Quote: "\u00f8" },  // å æ ø
  de:  { BracketLeft: "\u00fc", Semicolon: "\u00f6", Quote: "\u00e4", KeyZ: "y" },
  fr:  { KeyQ: "a", KeyW: "z", KeyA: "q", Semicolon: "m" },
  es:  { Quote: "\u00b4", BracketLeft: "`" },
  fi:  { BracketLeft: "\u00e5", Semicolon: "\u00f6", Quote: "\u00e4", Backslash: "'" },
  it:  { BracketLeft: "\u00e8", Quote: "\u00e0" },
  pt:  { BracketLeft: "+", Quote: "\u00ba" },
  gb:  { BracketLeft: "[", Semicolon: ";", Quote: "'", Backquote: "`" },
  us:  { BracketLeft: "[", Semicolon: ";", Quote: "'", Backquote: "`" },
};

/**
 * Detect keyboard layout using the Keyboard Layout Map API (Chrome/Edge).
 * Probes physical key mappings to identify the layout with high confidence.
 * Returns the XKB layout name or empty string if detection fails.
 */
async function detectKeyboardLayout(): Promise<string> {
  // Try Keyboard Layout Map API (Chrome/Edge only)
  if ("keyboard" in navigator && typeof (navigator as any).keyboard?.getLayoutMap === "function") {
    try {
      const layoutMap: Map<string, string> = await (navigator as any).keyboard.getLayoutMap();

      let bestLayout = "";
      let bestScore = 0;

      for (const [layout, signature] of Object.entries(LAYOUT_SIGNATURES)) {
        let matches = 0;
        let total = 0;
        for (const [key, expected] of Object.entries(signature)) {
          const actual = layoutMap.get(key);
          if (actual !== undefined) {
            total++;
            if (actual === expected) matches++;
          }
        }
        const score = total > 0 ? matches / total : 0;
        if (score > bestScore) {
          bestScore = score;
          bestLayout = layout;
        }
      }

      if (bestScore >= 0.6) {
        console.log(`Keyboard layout detected via Layout Map API: ${bestLayout} (score: ${bestScore})`);
        return bestLayout;
      }
    } catch {
      // API not available or permission denied
    }
  }

  // Fallback: check navigator.languages for non-English locale hints
  const LANG_MAP: Record<string, string> = {
    nb: "no", nn: "no", no: "no", sv: "se", da: "dk", de: "de",
    fr: "fr", es: "es", fi: "fi", pt: "pt", it: "it", nl: "nl",
    pl: "pl", ru: "ru", ja: "jp", ko: "kr", zh: "cn", cs: "cz",
    hu: "hu", ro: "ro", tr: "tr", uk: "ua", el: "gr", he: "il",
    ar: "ara", th: "th", is: "is",
  };
  for (const lang of navigator.languages || []) {
    const prefix = lang.toLowerCase().split("-")[0];
    if (prefix !== "en" && LANG_MAP[prefix]) {
      console.log(`Keyboard layout guessed from navigator.languages: ${LANG_MAP[prefix]} (${lang})`);
      return LANG_MAP[prefix];
    }
  }

  return "";
}

/**
 * Captures keyboard, mouse, and wheel events from the browser
 * and forwards them as compact InputEvents to the remote desktop.
 *
 * Mac keyboard remapping: Cmd is fully remapped to Ctrl. The Meta/Super
 * key is never sent to the remote (it would trigger WM actions).
 * Keys pressed while Cmd is held use complete press+release sequences
 * because Mac Chrome often skips keyup for Cmd combo keys.
 */
export class InputHandler {
  private target: HTMLElement;
  private videoElement: HTMLVideoElement | null;
  private localCursor: HTMLElement | null;
  private sendInput: (event: InputEvent) => void;
  private active = false;
  private pointerLocked = false;
  private scrollMultiplier = 1.0;

  /** When true, intercept browser shortcuts (Ctrl+W, Ctrl+T, etc.) and forward them to remote */
  forwardBrowserShortcuts = false;

  // Resize gating: Chrome's WebRTC H.264 decoder can't handle mid-stream
  // resolution changes. We suppress resize events until the first video
  // frame is decoded, and trigger a WebRTC reconnect for large resizes.
  private firstFrameReceived = false;
  private lastSentW = 0;
  private lastSentH = 0;
  private resizeNeededCallback: (() => void) | null = null;

  // Touch input state
  private longPressTimer: ReturnType<typeof setTimeout> | null = null;
  private touchStartX = 0;
  private touchStartY = 0;
  private longPressTriggered = false;
  private static readonly LONG_PRESS_MS = 500;
  private static readonly LONG_PRESS_MOVE_THRESHOLD = 10;

  // Bound listeners (stored so we can remove them)
  private onKeyDown = this.handleKeyDown.bind(this);
  private onKeyUp = this.handleKeyUp.bind(this);
  private onMouseMove = this.handleMouseMove.bind(this);
  private onMouseDown = this.handleMouseDown.bind(this);
  private onMouseUp = this.handleMouseUp.bind(this);
  private onWheel = this.handleWheel.bind(this);
  private onContextMenu = this.handleContextMenu.bind(this);
  private onFullscreenChange = this.handleFullscreenChange.bind(this);
  private onPointerLockChange = this.handlePointerLockChange.bind(this);
  private onTouchStart = this.handleTouchStart.bind(this);
  private onTouchMove = this.handleTouchMove.bind(this);
  private onTouchEnd = this.handleTouchEnd.bind(this);

  // Coalescing state
  private pendingMouseMove: { x: number; y: number } | null = null;
  private pendingRelativeMouseMove: { dx: number; dy: number } | null = null;
  private animationFrameId: number | null = null;

  private resizeObserver: ResizeObserver | null = null;
  private resizeTimer: ReturnType<typeof setTimeout> | null = null;

  constructor(target: HTMLElement, sendInput: (event: InputEvent) => void) {
    this.target = target;
    this.videoElement = target.querySelector("video");
    this.localCursor = document.getElementById("local-cursor");
    this.sendInput = sendInput;
  }

  /**
   * Send the current container dimensions as a resize immediately.
   * Called on every DataChannel open (including soft reconnects) to ensure
   * the agent has the correct resolution from the start. This breaks the
   * chicken-and-egg deadlock where the browser can't decode frames at the
   * wrong resolution, so requestVideoFrameCallback never fires, so no
   * resize is ever sent.
   */
  sendCurrentDimensions(): void {
    const rect = this.target.getBoundingClientRect();
    const w = roundToEven(rect.width);
    const h = roundToEven(rect.height);
    this.lastSentW = w;
    this.lastSentH = h;
    if (w > 0 && h > 0) {
      this.sendInput({ t: "r", w, h });
    }
  }

  /**
   * Notify that the first video frame has been decoded by Chrome.
   * Enables future resize events from ResizeObserver and fullscreen changes.
   */
  notifyFirstFrame(): void {
    this.firstFrameReceived = true;
  }

  /** Register callback invoked when a significant resize requires WebRTC reconnect */
  onResizeNeeded(callback: () => void): void {
    this.resizeNeededCallback = callback;
  }

  /** Send keyboard layout to remote agent. Uses saved preference, auto-detection, or fallback. */
  async sendLayout(): Promise<void> {
    const saved = localStorage.getItem("beam_keyboard_layout");
    let layout: string;
    if (saved) {
      layout = saved;
      console.log(`Using saved keyboard layout: ${layout}`);
    } else {
      layout = await detectKeyboardLayout();
      if (!layout) {
        layout = "us";
        console.log("Could not detect keyboard layout, defaulting to US");
      }
    }
    this.sendInput({ t: "l", layout });
    // Update the selector if it exists
    const select = document.getElementById("layout-select") as HTMLSelectElement | null;
    if (select && select.value !== layout) {
      select.value = layout;
    }
  }

  /** Send a specific keyboard layout to remote agent */
  sendSpecificLayout(layout: string): void {
    this.sendInput({ t: "l", layout });
  }

  /** Set the scroll speed multiplier (applied to wheel deltas before sending) */
  setScrollMultiplier(multiplier: number): void {
    this.scrollMultiplier = multiplier;
  }

  enable(): void {
    if (this.active) return;
    this.active = true;

    document.addEventListener("keydown", this.onKeyDown);
    document.addEventListener("keyup", this.onKeyUp);
    this.target.addEventListener("mousemove", this.onMouseMove);
    this.target.addEventListener("mousedown", this.onMouseDown);
    this.target.addEventListener("mouseup", this.onMouseUp);
    this.target.addEventListener("wheel", this.onWheel, { passive: false });
    this.target.addEventListener("contextmenu", this.onContextMenu);
    document.addEventListener("fullscreenchange", this.onFullscreenChange);
    document.addEventListener("pointerlockchange", this.onPointerLockChange);
    this.target.addEventListener("touchstart", this.onTouchStart, { passive: false });
    this.target.addEventListener("touchmove", this.onTouchMove, { passive: false });
    this.target.addEventListener("touchend", this.onTouchEnd, { passive: false });

    // Watch for container size changes and send resize events (debounced).
    this.resizeObserver = new ResizeObserver((entries) => {
      for (const entry of entries) {
        const w = Math.round(entry.contentRect.width);
        const h = Math.round(entry.contentRect.height);
        if (w > 0 && h > 0) {
          this.debouncedResize(w, h);
        }
      }
    });
    this.resizeObserver.observe(this.target);
  }

  disable(): void {
    if (!this.active) return;
    this.active = false;

    document.removeEventListener("keydown", this.onKeyDown);
    document.removeEventListener("keyup", this.onKeyUp);
    this.target.removeEventListener("mousemove", this.onMouseMove);
    this.target.removeEventListener("mousedown", this.onMouseDown);
    this.target.removeEventListener("mouseup", this.onMouseUp);
    this.target.removeEventListener("wheel", this.onWheel);
    this.target.removeEventListener("contextmenu", this.onContextMenu);
    document.removeEventListener("fullscreenchange", this.onFullscreenChange);
    document.removeEventListener("pointerlockchange", this.onPointerLockChange);
    this.target.removeEventListener("touchstart", this.onTouchStart);
    this.target.removeEventListener("touchmove", this.onTouchMove);
    this.target.removeEventListener("touchend", this.onTouchEnd);
    this.cancelLongPress();

    // Cancel pending mouse-move coalescing
    if (this.animationFrameId !== null) {
      cancelAnimationFrame(this.animationFrameId);
      this.animationFrameId = null;
    }
    this.pendingMouseMove = null;
    this.pendingRelativeMouseMove = null;

    // Hide local cursor
    this.localCursor?.classList.remove("visible");

    // Exit pointer lock if active
    if (this.pointerLocked && document.pointerLockElement) {
      document.exitPointerLock();
    }
    this.pointerLocked = false;
    this.firstFrameReceived = false;
    this.lastSentW = 0;
    this.lastSentH = 0;

    if (this.resizeObserver) {
      this.resizeObserver.disconnect();
      this.resizeObserver = null;
    }
    if (this.resizeTimer) {
      clearTimeout(this.resizeTimer);
      this.resizeTimer = null;
    }
  }

  // --- Keyboard ---

  private handleKeyDown(e: KeyboardEvent): void {
    // Browser shortcuts: forward to remote when enabled, otherwise let browser handle them
    if (isBrowserShortcut(e)) {
      if (!this.forwardBrowserShortcuts) return;
      e.preventDefault();
      // Fall through to send the key event to the remote desktop
    }

    // Don't capture when typing in input fields
    if (this.isInputElement(e.target)) return;

    // On Mac, suppress Meta (Cmd) key itself - it's remapped to Ctrl for
    // all shortcuts. Sending Meta to remote causes Super+Ctrl combos.
    if (isMac && (e.code === "MetaLeft" || e.code === "MetaRight")) {
      e.preventDefault();
      return;
    }

    // Mac Cmd+key → Ctrl+key on remote.
    // Send complete press+release because Mac Chrome often skips keyup
    // for keys pressed during Cmd combos.
    if (isMac && e.metaKey) {
      const evdev = keyCodeToEvdev(e.code);
      if (evdev === undefined) return;

      // Cmd+V: let the native paste event fire so ClipboardBridge can
      // read clipboardData reliably (navigator.clipboard.readText() often
      // fails on self-signed cert pages). Send Ctrl+V after a short delay
      // to give the agent time to set the X11 clipboard.
      if (e.key.toLowerCase() === "v") {
        // Don't preventDefault — let browser generate the paste event
        setTimeout(() => this.sendCtrlCombo(evdev), 50);
        return;
      }

      e.preventDefault();
      this.sendCtrlCombo(evdev);
      return;
    }

    // Non-Mac Ctrl+V: same approach — let paste event fire for clipboard sync
    if (!isMac && e.ctrlKey && e.key.toLowerCase() === "v") {
      const evdev = keyCodeToEvdev(e.code);
      if (evdev === undefined) return;
      setTimeout(() => this.sendCtrlCombo(evdev), 50);
      return;
    }

    const evdev = keyCodeToEvdev(e.code);
    if (evdev === undefined) return;

    e.preventDefault();
    this.sendInput({ t: "k", c: evdev, d: true });
  }

  private handleKeyUp(e: KeyboardEvent): void {
    if (isBrowserShortcut(e)) {
      if (!this.forwardBrowserShortcuts) return;
      e.preventDefault();
    }
    if (this.isInputElement(e.target)) return;

    // On Mac, suppress Meta key release (never sent to remote)
    if (isMac && (e.code === "MetaLeft" || e.code === "MetaRight")) {
      e.preventDefault();
      return;
    }

    // Mac Cmd+key combos already handled as press+release in keydown
    if (isMac && e.metaKey) {
      e.preventDefault();
      return;
    }

    const evdev = keyCodeToEvdev(e.code);
    if (evdev === undefined) return;

    e.preventDefault();
    this.sendInput({ t: "k", c: evdev, d: false });
  }

  // --- Keyboard helpers ---

  /** Send a complete Ctrl+key press+release sequence */
  private sendCtrlCombo(evdev: number): void {
    this.sendInput({ t: "k", c: EVDEV_LEFT_CTRL, d: true });
    this.sendInput({ t: "k", c: evdev, d: true });
    this.sendInput({ t: "k", c: evdev, d: false });
    this.sendInput({ t: "k", c: EVDEV_LEFT_CTRL, d: false });
  }


  // --- Mouse ---

  /**
   * Calculate normalized (0-1) coordinates within the actual video content area,
   * accounting for object-fit:contain letterboxing/pillarboxing.
   */
  private getVideoCoords(e: MouseEvent): { x: number; y: number } | null {
    const video = this.videoElement;
    if (!video || !video.videoWidth || !video.videoHeight) {
      const rect = this.target.getBoundingClientRect();
      return {
        x: Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width)),
        y: Math.max(0, Math.min(1, (e.clientY - rect.top) / rect.height)),
      };
    }

    const rect = video.getBoundingClientRect();
    const containerAspect = rect.width / rect.height;
    const videoAspect = video.videoWidth / video.videoHeight;

    let renderWidth: number, renderHeight: number, offsetX: number, offsetY: number;
    if (containerAspect > videoAspect) {
      renderHeight = rect.height;
      renderWidth = rect.height * videoAspect;
      offsetX = (rect.width - renderWidth) / 2;
      offsetY = 0;
    } else {
      renderWidth = rect.width;
      renderHeight = rect.width / videoAspect;
      offsetX = 0;
      offsetY = (rect.height - renderHeight) / 2;
    }

    const x = (e.clientX - rect.left - offsetX) / renderWidth;
    const y = (e.clientY - rect.top - offsetY) / renderHeight;

    return {
      x: Math.max(0, Math.min(1, x)),
      y: Math.max(0, Math.min(1, y)),
    };
  }

  private handleMouseMove(e: MouseEvent): void {
    if (this.pointerLocked) {
      // Pointer lock: aggregate pixel deltas
      const dx = e.movementX;
      const dy = e.movementY;
      if (dx !== 0 || dy !== 0) {
        if (!this.pendingRelativeMouseMove) {
          this.pendingRelativeMouseMove = { dx, dy };
        } else {
          this.pendingRelativeMouseMove.dx += dx;
          this.pendingRelativeMouseMove.dy += dy;
        }
        this.scheduleFrame();
      }
    } else {
      // Normal: send absolute coordinates
      const coords = this.getVideoCoords(e);
      if (coords) {
        this.pendingMouseMove = coords;
        this.scheduleFrame();
      }
    }
  }

  private scheduleFrame(): void {
    if (this.animationFrameId !== null) return;
    this.animationFrameId = requestAnimationFrame(() => {
      this.animationFrameId = null;
      if (this.pendingMouseMove) {
        this.sendInput({ t: "m", x: this.pendingMouseMove.x, y: this.pendingMouseMove.y });
        this.updateLocalCursor(this.pendingMouseMove.x, this.pendingMouseMove.y);
        this.pendingMouseMove = null;
      }
      if (this.pendingRelativeMouseMove) {
        this.sendInput({ t: "rm", dx: this.pendingRelativeMouseMove.dx, dy: this.pendingRelativeMouseMove.dy });
        this.pendingRelativeMouseMove = null;
      }
    });
  }

  /** Update local cursor visual position (0-1 normalized coordinates) */
  private updateLocalCursor(x: number, y: number): void {
    if (!this.localCursor || !this.videoElement || this.pointerLocked) return;

    const video = this.videoElement;
    const rect = video.getBoundingClientRect();
    const videoAspect = video.videoWidth / video.videoHeight || rect.width / rect.height;
    const containerAspect = rect.width / rect.height;

    let renderWidth: number, renderHeight: number, offsetX: number, offsetY: number;
    if (containerAspect > videoAspect) {
      renderHeight = rect.height;
      renderWidth = rect.height * videoAspect;
      offsetX = (rect.width - renderWidth) / 2;
      offsetY = 0;
    } else {
      renderWidth = rect.width;
      renderHeight = rect.width / videoAspect;
      offsetX = 0;
      offsetY = (rect.height - renderHeight) / 2;
    }

    const left = offsetX + x * renderWidth;
    const top = offsetY + y * renderHeight;

    this.localCursor.style.left = `${left}px`;
    this.localCursor.style.top = `${top}px`;
    this.localCursor.classList.add("visible");
  }

  private handleMouseDown(e: MouseEvent): void {
    e.preventDefault();
    const coords = this.getVideoCoords(e);
    if (coords) {
      // Send coordinates immediately for clicks to ensure accuracy
      this.sendInput({ t: "m", x: coords.x, y: coords.y });
      this.pendingMouseMove = null;
    }

    // Middle-click (button 1): try to sync browser clipboard to the remote
    // X11 PRIMARY selection BEFORE sending the button press. This enables
    // select-to-copy, middle-click-to-paste workflows for Linux users.
    // The clipboard read is async and may fail (permissions) — in that case
    // the button press is sent immediately without clipboard sync.
    if (e.button === 1) {
      this.sendPrimaryClipboardThenButton(e.button);
    } else {
      this.sendInput({ t: "b", b: e.button, d: true });
    }
  }

  /**
   * Read the browser clipboard and send it as PRIMARY selection, then
   * send the middle-click button press. If the clipboard read fails
   * (e.g. permissions denied, page not focused), send the button press
   * immediately without clipboard data.
   */
  private sendPrimaryClipboardThenButton(button: number): void {
    const MAX_CLIPBOARD_BYTES = 1_048_576; // 1 MB
    navigator.clipboard
      .readText()
      .then((text) => {
        if (text && text.length <= MAX_CLIPBOARD_BYTES) {
          this.sendInput({ t: "cp", text });
        }
        this.sendInput({ t: "b", b: button, d: true });
      })
      .catch(() => {
        // Clipboard read failed — send button press without clipboard sync
        this.sendInput({ t: "b", b: button, d: true });
      });
  }

  private handleMouseUp(e: MouseEvent): void {
    e.preventDefault();
    this.sendInput({ t: "b", b: e.button, d: false });
  }

  private handleWheel(e: WheelEvent): void {
    e.preventDefault();

    let dx = e.deltaX;
    let dy = e.deltaY;

    if (e.deltaMode === 1) {
      dx *= 30;
      dy *= 30;
    } else if (e.deltaMode === 2) {
      dx *= 300;
      dy *= 300;
    }

    dx *= this.scrollMultiplier;
    dy *= this.scrollMultiplier;

    this.sendInput({ t: "s", dx, dy });
  }

  private handleContextMenu(e: Event): void {
    e.preventDefault();
  }

  // --- Pointer Lock ---

  /** Programmatically toggle pointer lock (for use by external UI controls). */
  togglePointerLock(): void {
    if (this.pointerLocked) {
      document.exitPointerLock();
    } else {
      this.target.requestPointerLock();
    }
  }

  private handlePointerLockChange(): void {
    this.pointerLocked = document.pointerLockElement === this.target;
    if (this.pointerLocked && this.localCursor) {
      this.localCursor.classList.remove("visible");
    }
  }

  // --- Resize ---

  /**
   * On fullscreen change, send an immediate resize with correct dimensions.
   * Use screen dimensions for fullscreen (getBoundingClientRect may not
   * have settled yet), container dimensions when exiting.
   */
  private handleFullscreenChange(): void {
    if (!this.firstFrameReceived) return;
    // Give the browser time to settle the fullscreen layout
    setTimeout(() => {
      // Always measure the actual container — getBoundingClientRect gives
      // exact CSS pixel dimensions whether fullscreen or windowed.
      // screen.width/height can differ from the actual fullscreen area
      // (e.g. macOS notch, DPR scaling, rounding).
      const rect = this.target.getBoundingClientRect();
      let w = roundToEven(rect.width);
      let h = roundToEven(rect.height);

      // Re-assert cursor visibility after Chrome's fullscreen transition.
      // Chrome's user-agent fullscreen styles can hide the cursor.
      if (document.fullscreenElement) {
        this.target.style.cursor = "default";
        if (this.videoElement) {
          this.videoElement.style.cursor = "default";
        }
      }

      if (w > 0 && h > 0) {
        if (this.resizeTimer) {
          clearTimeout(this.resizeTimer);
          this.resizeTimer = null;
        }
        const significant = isSignificantResize(this.lastSentW, this.lastSentH, w, h);
        this.lastSentW = w;
        this.lastSentH = h;
        this.sendInput({ t: "r", w, h });
        if (significant) {
          this.resizeNeededCallback?.();
        }
      }
    }, 150);
  }

  private debouncedResize(w: number, h: number): void {
    if (!this.firstFrameReceived) return;
    if (this.resizeTimer) {
      clearTimeout(this.resizeTimer);
    }
    const ew = roundToEven(w);
    const eh = roundToEven(h);
    this.resizeTimer = setTimeout(() => {
      this.resizeTimer = null;
      const significant = isSignificantResize(this.lastSentW, this.lastSentH, ew, eh);
      this.lastSentW = ew;
      this.lastSentH = eh;
      this.sendInput({ t: "r", w: ew, h: eh });
      if (significant) {
        this.resizeNeededCallback?.();
      }
    }, 300);
  }

  // --- Touch input ---

  /**
   * Calculate normalized (0-1) coordinates within the actual video content area
   * from touch coordinates, accounting for object-fit:contain letterboxing.
   */
  private getTouchVideoCoords(touch: Touch): { x: number; y: number } | null {
    const video = this.videoElement;
    if (!video || !video.videoWidth || !video.videoHeight) {
      const rect = this.target.getBoundingClientRect();
      return {
        x: Math.max(0, Math.min(1, (touch.clientX - rect.left) / rect.width)),
        y: Math.max(0, Math.min(1, (touch.clientY - rect.top) / rect.height)),
      };
    }

    const rect = video.getBoundingClientRect();
    const containerAspect = rect.width / rect.height;
    const videoAspect = video.videoWidth / video.videoHeight;

    let renderWidth: number, renderHeight: number, offsetX: number, offsetY: number;
    if (containerAspect > videoAspect) {
      renderHeight = rect.height;
      renderWidth = rect.height * videoAspect;
      offsetX = (rect.width - renderWidth) / 2;
      offsetY = 0;
    } else {
      renderWidth = rect.width;
      renderHeight = rect.width / videoAspect;
      offsetX = 0;
      offsetY = (rect.height - renderHeight) / 2;
    }

    const x = (touch.clientX - rect.left - offsetX) / renderWidth;
    const y = (touch.clientY - rect.top - offsetY) / renderHeight;

    return {
      x: Math.max(0, Math.min(1, x)),
      y: Math.max(0, Math.min(1, y)),
    };
  }

  private handleTouchStart(e: TouchEvent): void {
    e.preventDefault();

    // Only handle single-finger touch for mouse emulation
    if (e.touches.length !== 1) {
      this.cancelLongPress();
      return;
    }

    const touch = e.touches[0];
    const coords = this.getTouchVideoCoords(touch);
    if (coords) {
      // Send coordinates immediately for clicks to ensure accuracy
      this.sendInput({ t: "m", x: coords.x, y: coords.y });
      this.pendingMouseMove = null;
    }

    // Start long-press timer for right-click
    this.touchStartX = touch.clientX;
    this.touchStartY = touch.clientY;
    this.longPressTriggered = false;
    this.cancelLongPress();
    this.longPressTimer = setTimeout(() => {
      this.longPressTriggered = true;
      // Send right-click (button 2) press + release
      if (coords) {
        this.sendInput({ t: "m", x: coords.x, y: coords.y });
      }
      this.sendInput({ t: "b", b: 2, d: true });
      this.sendInput({ t: "b", b: 2, d: false });
    }, InputHandler.LONG_PRESS_MS);

    // Send left button down
    this.sendInput({ t: "b", b: 0, d: true });
  }

  private handleTouchMove(e: TouchEvent): void {
    e.preventDefault();

    if (e.touches.length !== 1) {
      this.cancelLongPress();
      return;
    }

    const touch = e.touches[0];

    // Cancel long press if finger moved too far
    const dx = touch.clientX - this.touchStartX;
    const dy = touch.clientY - this.touchStartY;
    if (Math.sqrt(dx * dx + dy * dy) > InputHandler.LONG_PRESS_MOVE_THRESHOLD) {
      this.cancelLongPress();
    }

    const coords = this.getTouchVideoCoords(touch);
    if (coords) {
      this.pendingMouseMove = coords;
      this.scheduleFrame();
    }
  }

  private handleTouchEnd(e: TouchEvent): void {
    e.preventDefault();
    this.cancelLongPress();

    // Don't send button up if long press fired (it already sent right-click)
    if (!this.longPressTriggered) {
      this.sendInput({ t: "b", b: 0, d: false });
    }
    this.longPressTriggered = false;
  }

  private cancelLongPress(): void {
    if (this.longPressTimer) {
      clearTimeout(this.longPressTimer);
      this.longPressTimer = null;
    }
  }

  private isInputElement(target: EventTarget | null): boolean {
    if (!target || !(target instanceof HTMLElement)) return false;
    const tag = target.tagName;
    return (
      tag === "INPUT" ||
      tag === "TEXTAREA" ||
      tag === "SELECT" ||
      target.isContentEditable
    );
  }
}
