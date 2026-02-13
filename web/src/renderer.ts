/**
 * Manages video display and fullscreen for the remote desktop stream.
 * Includes FPS counting and local cursor rendering.
 */
export class Renderer {
  private videoElement: HTMLVideoElement;
  private containerElement: HTMLElement;
  private unmuted = false;

  // FPS tracking
  private frameCount = 0;
  private lastFpsTime = 0;
  private currentFps = 0;
  private fpsCallback: ((fps: number) => void) | null = null;
  private fpsInterval: ReturnType<typeof setInterval> | null = null;

  constructor(videoElement: HTMLVideoElement, containerElement: HTMLElement) {
    this.videoElement = videoElement;
    this.containerElement = containerElement;

    // Handle autoplay: browsers require muted autoplay, unmute on first click
    // on the desktop view. Using containerElement (not document) so that clicks
    // on the login form don't consume the one-shot listener.
    this.videoElement.muted = true;
    this.containerElement.addEventListener(
      "click",
      () => {
        if (!this.unmuted) {
          this.videoElement.muted = false;
          this.unmuted = true;
        }
      },
      { once: true },
    );
  }

  /** Attach a remote MediaStream to the video element */
  hasStream(): boolean {
    return this.videoElement.srcObject !== null;
  }

  attachStream(stream: MediaStream): void {
    this.videoElement.srcObject = stream;

    // Tell Chrome we want lowest-latency decoding (reduces internal buffering)
    (this.videoElement as any).latencyHint = "realtime";

    this.videoElement.onerror = () => {
      console.error("Video element error:", this.videoElement.error);
    };

    this.videoElement.play().catch((err) => {
      console.warn("Autoplay blocked:", err);
    });

    // Use default cursor - the remote desktop cursor is visible in the
    // video stream. The browser cursor shows where input events land.
    this.videoElement.style.cursor = "default";

    this.startFpsCounter();
  }

  /** Get current FPS value */
  getFps(): number {
    return this.currentFps;
  }

  /** Set callback to receive FPS updates */
  onFpsUpdate(callback: (fps: number) => void): void {
    this.fpsCallback = callback;
  }

  /** Enter fullscreen mode */
  enterFullscreen(): void {
    this.containerElement.requestFullscreen?.().catch((err) => {
      console.warn("Fullscreen request failed:", err);
    });
  }

  /** Exit fullscreen mode */
  exitFullscreen(): void {
    if (document.fullscreenElement) {
      document.exitFullscreen?.();
    }
  }

  /** Clean up resources */
  destroy(): void {
    this.stopFpsCounter();
  }

  private startFpsCounter(): void {
    this.stopFpsCounter();

    const video = this.videoElement as HTMLVideoElement & {
      requestVideoFrameCallback?: (cb: () => void) => void;
    };

    if (video.requestVideoFrameCallback) {
      // Use requestVideoFrameCallback for accurate frame counting
      this.lastFpsTime = performance.now();
      this.frameCount = 0;

      const countFrame = (): void => {
        this.frameCount++;
        video.requestVideoFrameCallback!(countFrame);
      };
      video.requestVideoFrameCallback(countFrame);

      this.fpsInterval = setInterval(() => {
        const now = performance.now();
        const elapsed = (now - this.lastFpsTime) / 1000;
        if (elapsed > 0) {
          this.currentFps = this.frameCount / elapsed;
          this.fpsCallback?.(this.currentFps);
        }
        this.frameCount = 0;
        this.lastFpsTime = now;
      }, 1000);
    } else {
      // Fallback: estimate from video metadata
      this.fpsInterval = setInterval(() => {
        const stream = this.videoElement.srcObject as MediaStream | null;
        const track = stream?.getVideoTracks()[0];
        if (track) {
          const settings = track.getSettings();
          if (settings.frameRate) {
            this.currentFps = settings.frameRate;
            this.fpsCallback?.(this.currentFps);
          }
        }
      }, 1000);
    }
  }

  private stopFpsCounter(): void {
    if (this.fpsInterval) {
      clearInterval(this.fpsInterval);
      this.fpsInterval = null;
    }
  }
}
