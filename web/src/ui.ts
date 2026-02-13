/**
 * Status bar and notification system for the Beam remote desktop client.
 *
 * Single bottom bar with connection status, stats, and controls.
 * In fullscreen the bar auto-hides and shows on mouse hover near the bottom edge.
 */

type NotificationType = "info" | "error" | "success";

const NOTIFICATION_DURATION_MS = 4_000;
const FS_BAR_HIDE_DELAY_MS = 2_000;
const FS_ACTIVATION_ZONE_PX = 6;

export class BeamUI {
  private statusBar: HTMLElement;
  private latencyEl: HTMLElement;
  private fpsEl: HTMLElement;
  private qualityEl: HTMLElement;
  private notificationContainer: HTMLElement;

  private onFullscreen: (() => void) | null = null;
  private onDisconnect: (() => void) | null = null;
  private onEndSession: (() => void) | null = null;

  private fsHideTimer: ReturnType<typeof setTimeout> | null = null;
  private isFullscreen = false;

  constructor() {
    this.statusBar = document.getElementById("status-bar") as HTMLElement;
    this.latencyEl = document.getElementById("stat-latency") as HTMLElement;
    this.fpsEl = document.getElementById("stat-fps") as HTMLElement;
    this.qualityEl = document.getElementById("stat-quality") as HTMLElement;
    this.notificationContainer = document.getElementById("notifications") as HTMLElement;

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

  updateLatency(ms: number): void {
    this.latencyEl.textContent = `${Math.round(ms)} ms`;
  }

  updateFps(fps: number): void {
    this.fpsEl.textContent = `${Math.round(fps)} FPS`;
  }

  updateQuality(
    bitrateKbps: number | null,
    lossPercent: number,
    candidateType: string,
  ): void {
    const parts: string[] = [];
    if (bitrateKbps !== null) {
      parts.push(
        bitrateKbps >= 1000
          ? `${(bitrateKbps / 1000).toFixed(1)} Mbps`
          : `${bitrateKbps} kbps`,
      );
    }
    if (lossPercent > 0) {
      parts.push(`${lossPercent}% loss`);
    }
    if (candidateType === "relay") {
      parts.push("TURN");
    }
    this.qualityEl.textContent = parts.join(" | ");
  }

  showNotification(message: string, type: NotificationType): void {
    const el = document.createElement("div");
    el.className = `notification notification-${type}`;
    el.textContent = message;
    this.notificationContainer.appendChild(el);

    void el.offsetWidth;
    el.classList.add("visible");

    setTimeout(() => {
      el.classList.remove("visible");
      el.addEventListener("transitionend", () => el.remove(), { once: true });
      setTimeout(() => el.remove(), 500);
    }, NOTIFICATION_DURATION_MS);
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
