/**
 * WebCodecs-based video/audio renderer for the Beam remote desktop client.
 * Video frames arrive as H.264 Annex B payloads over WebSocket binary messages,
 * are decoded via VideoDecoder, and drawn to a <canvas> via drawImage(VideoFrame).
 *
 * Audio frames arrive as Opus payloads, decoded via AudioDecoder, and played
 * through an AudioContext.
 */
export class WebCodecsRenderer {
  private decoder: VideoDecoder | null = null;
  private audioDecoder: AudioDecoder | null = null;
  private audioContext: AudioContext | null = null;
  private canvas: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D;
  private containerElement: HTMLElement;
  private currentWidth = 0;
  private currentHeight = 0;
  private framesDecoded = 0;
  private prevFrameCount = 0;
  private currentFps = 0;
  private fpsInterval: ReturnType<typeof setInterval> | null = null;
  private audioMuted = true;
  private nextAudioPlayTime = 0;
  private muteChangeCallback: ((muted: boolean) => void) | null = null;
  private fpsCallback: ((fps: number, decodeMs: number) => void) | null = null;
  private firstFrameCallback: (() => void) | null = null;
  private firstFrameFired = false;
  private lastFeedTimeMs = 0;
  private decodeTimeMs = 0;
  private needsKeyframe = true;
  private videoFrameCount = 0;
  private audioFrameCount = 0;

  constructor(canvas: HTMLCanvasElement, containerElement: HTMLElement) {
    this.canvas = canvas;
    this.containerElement = containerElement;
    const ctx = canvas.getContext("2d");
    if (!ctx) throw new Error("Failed to get 2d context from canvas");
    this.ctx = ctx;

    // Auto-unmute on first desktop click only if user has never set a preference.
    // Returning users get their saved preference restored by main.ts instead.
    if (localStorage.getItem("beam_audio_muted") === null) {
      this.containerElement.addEventListener(
        "click",
        () => {
          if (this.audioMuted) {
            this.setAudioMuted(false);
          }
        },
        { once: true },
      );
    }
  }

  /** Register callback for the first decoded video frame */
  onFirstFrame(callback: () => void): void {
    this.firstFrameCallback = callback;
  }

  /** Register callback for mute state changes */
  onMuteChange(callback: (muted: boolean) => void): void {
    this.muteChangeCallback = callback;
  }

  /** Returns true if audio is currently muted */
  isMuted(): boolean {
    return this.audioMuted;
  }

  /** Toggle audio mute state. Returns the new muted state. */
  toggleMute(): boolean {
    this.setAudioMuted(!this.audioMuted);
    return this.audioMuted;
  }

  /** Set audio mute state directly */
  setAudioMuted(muted: boolean): void {
    this.audioMuted = muted;
    if (!muted && !this.audioContext) {
      this.audioContext = new AudioContext({ sampleRate: 48000 });
    }
    if (this.audioContext) {
      if (muted) {
        this.audioContext.suspend();
      } else {
        this.audioContext.resume();
      }
    }
    this.muteChangeCallback?.(muted);
  }

  /** Returns true if we have received at least one frame */
  hasStream(): boolean {
    return this.firstFrameFired;
  }

  /** Get the canvas element (for screenshot capture) */
  getCanvas(): HTMLCanvasElement {
    return this.canvas;
  }

  /** Get current video width */
  getVideoWidth(): number {
    return this.currentWidth;
  }

  /** Get current video height */
  getVideoHeight(): number {
    return this.currentHeight;
  }

  /** Configure or reconfigure the video decoder for the given resolution */
  private configureDecoder(width: number, height: number): void {
    console.log(`[Beam] configureDecoder: ${width}x${height}`);
    if (this.decoder) {
      this.decoder.close();
      this.decoder = null;
    }

    this.currentWidth = width;
    this.currentHeight = height;
    this.canvas.width = width;
    this.canvas.height = height;

    this.decoder = new VideoDecoder({
      output: (frame: VideoFrame) => {
        this.ctx.drawImage(frame, 0, 0);
        frame.close();
        this.framesDecoded++;
        this.decodeTimeMs = performance.now() - this.lastFeedTimeMs;

        if (!this.firstFrameFired) {
          console.log(`[Beam] First video frame decoded: ${frame.displayWidth}x${frame.displayHeight}`);
          this.firstFrameFired = true;
          this.firstFrameCallback?.();
        }
      },
      error: (err: DOMException) => {
        console.error("VideoDecoder error:", err);
      },
    });

    this.decoder.configure({
      codec: "avc1.4d0033", // Main profile, Level 5.1
      hardwareAcceleration: "prefer-hardware",
      optimizeForLatency: true,
    });
    this.needsKeyframe = true;

    this.startFpsCounter();
  }

  /** Feed a video frame from the binary WebSocket message */
  feedVideoFrame(
    flags: number,
    width: number,
    height: number,
    timestampUs: bigint,
    payload: Uint8Array,
  ): void {
    this.videoFrameCount++;
    if (this.videoFrameCount <= 5) {
      const isKf = (flags & 0x01) !== 0;
      console.log(`[Beam] feedVideoFrame #${this.videoFrameCount}: ${width}x${height} flags=0x${flags.toString(16)} keyframe=${isKf} payload=${payload.byteLength} decoderState=${this.decoder?.state ?? "null"}`);
    }

    // Reconfigure decoder if resolution changed
    if (width !== this.currentWidth || height !== this.currentHeight) {
      this.configureDecoder(width, height);
    }

    if (!this.decoder || this.decoder.state === "closed") return;

    const isKeyframe = (flags & 0x01) !== 0;

    // If decoder not yet configured, skip
    if (this.decoder.state !== "configured") return;

    // After configure() or flush(), decoder requires a keyframe first
    if (this.needsKeyframe && !isKeyframe) return;
    if (isKeyframe) this.needsKeyframe = false;

    const chunk = new EncodedVideoChunk({
      type: isKeyframe ? "key" : "delta",
      timestamp: Number(timestampUs),
      data: payload,
    });

    try {
      this.lastFeedTimeMs = performance.now();
      this.decoder.decode(chunk);
    } catch (err) {
      console.error("VideoDecoder.decode() error:", err);
    }
  }

  /** Feed an audio frame from the binary WebSocket message */
  feedAudioFrame(timestampUs: bigint, payload: Uint8Array): void {
    if (this.audioMuted || !this.audioContext) return;

    this.audioFrameCount++;
    if (this.audioFrameCount === 1) {
      console.log("Audio: first frame received", {
        payloadSize: payload.byteLength,
        audioContextState: this.audioContext.state,
      });
    }

    if (!this.audioDecoder) {
      this.nextAudioPlayTime = 0;
      let firstDecodeLogged = false;
      this.audioDecoder = new AudioDecoder({
        output: (audioData: AudioData) => {
          if (!firstDecodeLogged) {
            console.log("Audio: first decode successful", {
              frames: audioData.numberOfFrames,
              channels: audioData.numberOfChannels,
              sampleRate: audioData.sampleRate,
            });
            firstDecodeLogged = true;
          }
          // Play audio via AudioContext
          if (this.audioContext && this.audioContext.state === "running") {
            const numFrames = audioData.numberOfFrames;
            const numChannels = audioData.numberOfChannels;
            const sampleRate = audioData.sampleRate;
            const buffer = this.audioContext.createBuffer(numChannels, numFrames, sampleRate);

            for (let ch = 0; ch < numChannels; ch++) {
              const channelData = buffer.getChannelData(ch);
              audioData.copyTo(channelData, { planeIndex: ch, format: "f32-planar" });
            }

            const source = this.audioContext.createBufferSource();
            source.buffer = buffer;
            source.connect(this.audioContext.destination);

            const now = this.audioContext.currentTime;
            // Snap forward if we've fallen behind (network stall, tab resume)
            if (this.nextAudioPlayTime < now) {
              this.nextAudioPlayTime = now;
            }
            source.start(this.nextAudioPlayTime);
            this.nextAudioPlayTime += buffer.duration;
          }
          audioData.close();
        },
        error: (err: DOMException) => {
          console.error("AudioDecoder error:", err);
        },
      });

      this.audioDecoder.configure({
        codec: "opus",
        sampleRate: 48000,
        numberOfChannels: 2,
      });
    }

    if (this.audioDecoder.state !== "configured") return;

    const chunk = new EncodedAudioChunk({
      type: "key", // Opus frames are always independently decodable
      timestamp: Number(timestampUs),
      data: payload,
    });

    try {
      this.audioDecoder.decode(chunk);
    } catch (err) {
      console.error("AudioDecoder.decode() error:", err);
    }
  }

  /** Get current FPS value */
  getFps(): number {
    return this.currentFps;
  }

  /** Set callback to receive FPS and decode time updates */
  onFpsUpdate(callback: (fps: number, decodeMs: number) => void): void {
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
    if (this.decoder) {
      try { this.decoder.close(); } catch { /* already closed */ }
      this.decoder = null;
    }
    if (this.audioDecoder) {
      try { this.audioDecoder.close(); } catch { /* already closed */ }
      this.audioDecoder = null;
    }
    this.nextAudioPlayTime = 0;
    if (this.audioContext) {
      this.audioContext.close();
      this.audioContext = null;
    }
    this.firstFrameFired = false;
    this.currentWidth = 0;
    this.currentHeight = 0;
  }

  private startFpsCounter(): void {
    this.stopFpsCounter();
    this.prevFrameCount = this.framesDecoded;

    this.fpsInterval = setInterval(() => {
      const decoded = this.framesDecoded - this.prevFrameCount;
      this.currentFps = decoded;
      this.prevFrameCount = this.framesDecoded;
      this.fpsCallback?.(this.currentFps, this.decodeTimeMs);
    }, 1000);
  }

  private stopFpsCounter(): void {
    if (this.fpsInterval) {
      clearInterval(this.fpsInterval);
      this.fpsInterval = null;
    }
  }
}
