/**
 * Status bar and notification system for the Beam remote desktop client.
 *
 * Single bottom bar with connection status, stats, and controls.
 * In fullscreen the bar auto-hides and shows on mouse hover near the bottom edge.
 */

type NotificationType = "info" | "error" | "success" | "warning";

const TOAST_DURATION_MS = 3_000;
const FS_BAR_HIDE_DELAY_MS = 2_000;
const FS_ACTIVATION_ZONE_PX = 6;

export class BeamUI {
  private statusBar: HTMLElement;
  private toastContainer: HTMLElement;

  private onFullscreen: (() => void) | null = null;
  private onDisconnect: (() => void) | null = null;
  private onEndSession: (() => void) | null = null;

  private fsHideTimer: ReturnType<typeof setTimeout> | null = null;
  private isFullscreen = false;

  constructor() {
    this.statusBar = document.getElementById("status-bar") as HTMLElement;
    this.toastContainer = document.getElementById("toast-container") as HTMLElement;

    this.setupButtons();
    this.setupFullscreenAutoHide();
  }

  setOnFullscreen(callback: () => void): void {
    this.onFullscreen = callback;
  }

  setOnDisconnect(callback: () => void): void {
    this.onDisconnect = callback;
  }

  setOnEndSession(callback: () => void): void {
    this.onEndSession = callback;
  }

  showNotification(message: string, type: NotificationType, durationMs: number = TOAST_DURATION_MS): void {
    const el = document.createElement("div");
    el.className = `toast toast-${type}`;

    const msgSpan = document.createElement("span");
    msgSpan.className = "toast-message";
    msgSpan.textContent = message;
    el.appendChild(msgSpan);

    const closeBtn = document.createElement("button");
    closeBtn.className = "toast-close";
    closeBtn.setAttribute("aria-label", "Dismiss notification");
    closeBtn.textContent = "\u00D7";
    closeBtn.addEventListener("click", () => dismissToast(el));
    el.appendChild(closeBtn);

    this.toastContainer.appendChild(el);

    // Force reflow so the transition plays from opacity 0 -> 1
    void el.offsetWidth;
    el.classList.add("visible");

    const timer = setTimeout(() => dismissToast(el), durationMs);

    function dismissToast(toast: HTMLElement): void {
      clearTimeout(timer);
      toast.classList.remove("visible");
      toast.addEventListener("transitionend", () => toast.remove(), { once: true });
      // Fallback removal if transitionend never fires
      setTimeout(() => toast.remove(), 500);
    }
  }

  private setupButtons(): void {
    document.getElementById("btn-fullscreen")?.addEventListener("click", () => {
      this.onFullscreen?.();
    });
    document.getElementById("btn-disconnect")?.addEventListener("click", () => {
      this.onDisconnect?.();
    });
    document.getElementById("btn-end-session")?.addEventListener("click", () => {
      this.onEndSession?.();
    });
  }

  private setupFullscreenAutoHide(): void {
    document.addEventListener("fullscreenchange", () => {
      this.isFullscreen = !!document.fullscreenElement;
      if (this.isFullscreen) {
        this.statusBar.classList.add("fs-hidden");
        this.statusBar.classList.remove("fs-peek");
      } else {
        this.statusBar.classList.remove("fs-hidden", "fs-peek");
        this.clearFsHideTimer();
      }
    });

    document.addEventListener("mousemove", (e: MouseEvent) => {
      if (!this.isFullscreen) return;
      if (e.clientY >= window.innerHeight - FS_ACTIVATION_ZONE_PX) {
        this.statusBar.classList.add("fs-peek");
        this.clearFsHideTimer();
      }
    });

    this.statusBar.addEventListener("mouseenter", () => {
      if (!this.isFullscreen) return;
      this.statusBar.classList.add("fs-peek");
      this.clearFsHideTimer();
    });

    this.statusBar.addEventListener("mouseleave", () => {
      if (!this.isFullscreen) return;
      this.resetFsHideTimer();
    });
  }

  private resetFsHideTimer(): void {
    this.clearFsHideTimer();
    this.fsHideTimer = setTimeout(() => {
      this.statusBar.classList.remove("fs-peek");
    }, FS_BAR_HIDE_DELAY_MS);
  }

  private clearFsHideTimer(): void {
    if (this.fsHideTimer) {
      clearTimeout(this.fsHideTimer);
      this.fsHideTimer = null;
    }
  }
}
