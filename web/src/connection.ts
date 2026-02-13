/**
 * Signaling messages matching the Rust SignalingMessage enum.
 * Uses snake_case serde tag: "offer", "answer", "ice_candidate", "error"
 */
type SignalMessage =
  | { type: "offer"; sdp: string; session_id: string }
  | { type: "answer"; sdp: string; session_id: string }
  | {
      type: "ice_candidate";
      candidate: string;
      sdp_mid: string | null;
      sdp_mline_index: number | null;
      session_id: string;
    }
  | { type: "error"; message: string };

/**
 * Input events sent over the DataChannel.
 * Compact wire format matching the Rust InputEvent enum (serde tag = "t").
 */
export type InputEvent =
  | { t: "k"; c: number; d: boolean }
  | { t: "m"; x: number; y: number }
  | { t: "rm"; dx: number; dy: number }
  | { t: "b"; b: number; d: boolean }
  | { t: "s"; dx: number; dy: number }
  | { t: "c"; text: string }
  | { t: "cp"; text: string }
  | { t: "r"; w: number; h: number }
  | { t: "l"; layout: string }
  | { t: "q"; mode: string }
  | { t: "vs"; visible: boolean }
  | { t: "cur"; css: string }
  | { t: "fs"; id: string; name: string; size: number }
  | { t: "fc"; id: string; data: string }
  | { t: "fd"; id: string };

type TrackCallback = (stream: MediaStream) => void;
type VoidCallback = () => void;
type CountdownCallback = (secondsRemaining: number) => void;

const DEFAULT_ICE_SERVERS: RTCIceServer[] = [
  { urls: "stun:stun.l.google.com:19302" },
];

const MAX_RECONNECT_DELAY_MS = 30_000;
const BASE_RECONNECT_DELAY_MS = 1_000;
const MAX_RECONNECT_ATTEMPTS = 10;

/**
 * Manages WebRTC connection to the Beam server via WebSocket signaling.
 */
export class BeamConnection {
  private sessionId: string;
  private token: string;
  private ws: WebSocket | null = null;
  private pc: RTCPeerConnection | null = null;
  private dataChannel: RTCDataChannel | null = null;
  private reconnectAttempt = 0;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private offerRetryTimer: ReturnType<typeof setTimeout> | null = null;
  private offerRetryCount = 0;
  private intentionalDisconnect = false;
  private iceDisconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private pendingOfferSdp: string | null = null;

  private trackCallback: TrackCallback | null = null;
  private dataChannelOpenCallback: VoidCallback | null = null;
  private dataChannelMessageCallback: ((event: InputEvent) => void) | null = null;
  private disconnectCallback: VoidCallback | null = null;
  private reconnectingCallback: ((attempt: number, maxAttempts: number) => void) | null = null;
  private reconnectFailedCallback: VoidCallback | null = null;
  private autoReconnectingCallback: CountdownCallback | null = null;
  private autoReconnectRecoveredCallback: VoidCallback | null = null;
  private replacedCallback: VoidCallback | null = null;
  private agentExitedCallback: VoidCallback | null = null;
  private iceAutoReconnectCountdown: ReturnType<typeof setInterval> | null = null;
  private iceServers: RTCIceServer[] = DEFAULT_ICE_SERVERS;

  constructor(sessionId: string, token: string) {
    this.sessionId = sessionId;
    this.token = token;
  }

  /** Register callback for when a remote media track is received */
  onTrack(callback: TrackCallback): void {
    this.trackCallback = callback;
  }

  /** Register callback for when the input data channel opens */
  onDataChannelOpen(callback: VoidCallback): void {
    this.dataChannelOpenCallback = callback;
  }

  /** Register callback for messages received from the agent via data channel */
  onDataChannelMessage(callback: (event: InputEvent) => void): void {
    this.dataChannelMessageCallback = callback;
  }

  /** Register callback for when the connection is lost */
  onDisconnect(callback: VoidCallback): void {
    this.disconnectCallback = callback;
  }

  /** Register callback for reconnection attempts */
  onReconnecting(callback: (attempt: number, maxAttempts: number) => void): void {
    this.reconnectingCallback = callback;
  }

  /** Register callback for when all reconnection attempts have been exhausted */
  onReconnectFailed(callback: VoidCallback): void {
    this.reconnectFailedCallback = callback;
  }

  /** Register callback for auto-reconnect countdown (ICE disconnected/failed).
   *  Called each second with the remaining seconds (3, 2, 1, 0).
   *  0 means the auto-reconnect is now triggering. */
  onAutoReconnecting(callback: CountdownCallback): void {
    this.autoReconnectingCallback = callback;
  }

  /** Register callback for when ICE self-recovers during the countdown */
  onAutoReconnectRecovered(callback: VoidCallback): void {
    this.autoReconnectRecoveredCallback = callback;
  }

  /** Register callback for when this tab was replaced by another tab/window */
  onReplaced(callback: VoidCallback): void {
    this.replacedCallback = callback;
  }

  /** Register callback for when the agent process exited unexpectedly */
  onAgentExited(callback: VoidCallback): void {
    this.agentExitedCallback = callback;
  }

  /** Update the token (after refresh) so reconnections use the new one */
  updateToken(token: string): void {
    this.token = token;
  }

  /** Establish WebSocket + WebRTC connection */
  async connect(): Promise<void> {
    this.intentionalDisconnect = false;
    this.reconnectAttempt = 0;
    await this.fetchIceConfig();
    await this.establishConnection();
  }

  /** Fetch ICE/TURN server config from the server */
  private async fetchIceConfig(): Promise<void> {
    try {
      const resp = await fetch("/api/ice-config", {
        headers: { Authorization: `Bearer ${this.token}` },
      });
      if (!resp.ok) return; // Fall back to defaults

      const data = (await resp.json()) as {
        ice_servers: Array<{
          urls: string[];
          username?: string;
          credential?: string;
        }>;
      };

      if (data.ice_servers?.length > 0) {
        this.iceServers = data.ice_servers.map((s) => ({
          urls: s.urls,
          username: s.username,
          credential: s.credential,
        }));
      }
    } catch {
      // Use defaults on failure
    }
  }

  /** Cleanly tear down the connection */
  disconnect(): void {
    this.intentionalDisconnect = true;
    this.cleanup();
  }

  /** Send an input event over the data channel */
  sendInput(event: InputEvent): void {
    if (this.dataChannel?.readyState === "open") {
      this.dataChannel.send(JSON.stringify(event));
    }
  }

  /**
   * Soft reconnect: close RTCPeerConnection (keep WebSocket open),
   * create a new PeerConnection and send a new SDP offer.
   * Used after resolution changes — Chrome's WebRTC H.264 decoder
   * can't handle mid-stream resolution changes, so we need a fresh
   * decoder context via a new peer connection.
   */
  softReconnect(): void {
    if (!this.ws || this.ws.readyState !== WebSocket.OPEN) return;

    console.log("Soft reconnect: recreating PeerConnection for resolution change");

    if (this.offerRetryTimer) {
      clearTimeout(this.offerRetryTimer);
      this.offerRetryTimer = null;
    }
    this.offerRetryCount = 0;
    this.pendingOfferSdp = null;

    this.cancelIceAutoReconnect();

    if (this.dataChannel) {
      this.dataChannel.close();
      this.dataChannel = null;
    }

    if (this.pc) {
      this.pc.close();
      this.pc = null;
    }

    this.setupPeerConnection();
  }

  /** Get WebRTC stats for monitoring latency and connection quality */
  async getStats(): Promise<RTCStatsReport | null> {
    if (!this.pc) return null;
    return this.pc.getStats();
  }

  private async establishConnection(): Promise<void> {
    this.cleanup();

    const wsProtocol = location.protocol === "https:" ? "wss:" : "ws:";
    const wsUrl = `${wsProtocol}//${location.host}/api/sessions/${this.sessionId}/ws?token=${encodeURIComponent(this.token)}`;

    this.ws = new WebSocket(wsUrl);

    let wsOpened = false;

    this.ws.onopen = () => {
      wsOpened = true;
      this.reconnectAttempt = 0;
      this.setupPeerConnection();
    };

    this.ws.onmessage = (event: MessageEvent) => {
      const msg = JSON.parse(event.data as string) as SignalMessage;
      this.handleSignalMessage(msg);
    };

    this.ws.onclose = (event: CloseEvent) => {
      if (!this.intentionalDisconnect) {
        // If the WebSocket never opened, the server rejected the upgrade
        // (likely 401 expired/invalid token). Don't retry — go back to login.
        if (!wsOpened && event.code === 1006) {
          console.error("WebSocket rejected (likely auth failure), not retrying");
          this.reconnectFailedCallback?.();
          return;
        }
        this.scheduleReconnect();
      }
      this.disconnectCallback?.();
    };

    this.ws.onerror = () => {
      // onclose will fire after onerror, reconnect handled there
    };
  }

  private setupPeerConnection(): void {
    this.pc = new RTCPeerConnection({ iceServers: this.iceServers });

    // Create data channel for input before creating offer
    // Unordered + no retransmits: input events behave like UDP.
    // A stale mouse-move arriving late is worse than a dropped one (cursor jumps back).
    // Dropped events are invisible since the next one arrives ~16ms later.
    this.dataChannel = this.pc.createDataChannel("input", {
      ordered: false,
      maxRetransmits: 0,
    });
    this.dataChannel.onopen = () => {
      this.dataChannelOpenCallback?.();
    };
    this.dataChannel.onmessage = (event: MessageEvent) => {
      try {
        const msg = JSON.parse(event.data as string) as InputEvent;
        this.dataChannelMessageCallback?.(msg);
      } catch { /* ignore malformed messages */ }
    };

    this.pc.ontrack = (event: RTCTrackEvent) => {
      console.log("ontrack fired", {
        kind: event.track.kind,
        id: event.track.id,
        readyState: event.track.readyState,
        muted: event.track.muted,
        streamsLength: event.streams?.length,
      });

      // Monitor track state changes
      event.track.onmute = () => console.warn("Track muted:", event.track.kind);
      event.track.onunmute = () => console.log("Track unmuted:", event.track.kind);
      event.track.onended = () => console.warn("Track ended:", event.track.kind);

      // Minimize jitter buffer for low-latency remote desktop.
      // Apply immediately AND re-apply after a delay because Chrome can
      // reset these hints during SDP negotiation or track setup.
      if (event.track.kind === "video" || event.track.kind === "audio") {
        const applyJitterHints = () => {
          const receiver = this.pc?.getReceivers().find(r => r.track === event.track);
          if (receiver) {
            (receiver as any).playoutDelayHint = 0;
            try { (receiver as any).jitterBufferTarget = 0; } catch { /* unsupported */ }
          }
        };
        applyJitterHints();
        setTimeout(applyJitterHints, 500);
        setTimeout(applyJitterHints, 2000);
      }

      // Only forward the stream for video tracks.
      // ontrack fires separately for video and audio. If we forward both,
      // the second call (audio) could replace the video stream on the element.
      // When event.streams[0] exists, it already contains both audio and video tracks.
      if (event.track.kind === "video") {
        let stream: MediaStream;
        if (event.streams && event.streams.length > 0) {
          stream = event.streams[0];
        } else {
          console.warn("ontrack: streams array empty, creating MediaStream from track");
          stream = new MediaStream([event.track]);
        }
        this.trackCallback?.(stream);
      }
    };

    this.pc.onicecandidate = (event: RTCPeerConnectionIceEvent) => {
      if (event.candidate) {
        this.sendSignal({
          type: "ice_candidate",
          candidate: event.candidate.candidate,
          sdp_mid: event.candidate.sdpMid,
          sdp_mline_index: event.candidate.sdpMLineIndex,
          session_id: this.sessionId,
        });
      }
    };

    this.pc.oniceconnectionstatechange = () => {
      const state = this.pc?.iceConnectionState;
      console.log("ICE connection state:", state);

      if (state === "connected" || state === "completed") {
        // ICE recovered or connected: cancel any pending auto-reconnect
        const wasCountingDown = this.iceDisconnectTimer !== null || this.iceAutoReconnectCountdown !== null;
        this.cancelIceAutoReconnect();
        if (wasCountingDown) {
          console.log("ICE self-recovered, cancelling auto-reconnect");
          this.autoReconnectRecoveredCallback?.();
        }
      } else if (state === "disconnected" || state === "failed") {
        // Start auto-reconnect countdown. Both "disconnected" (transient) and
        // "failed" (permanent) get a 3-second grace period — this handles
        // network changes (Wi-Fi to Ethernet, roaming) without requiring manual
        // intervention, while still giving ICE a chance to self-recover.
        if (!this.iceDisconnectTimer && !this.intentionalDisconnect) {
          this.startIceAutoReconnect();
        }
      }
    };

    this.createAndSendOffer();
  }

  /** Start a 3-second auto-reconnect countdown. Notifies the UI each second
   *  so it can display a countdown. If ICE recovers during the countdown,
   *  cancelIceAutoReconnect() cancels everything. */
  private startIceAutoReconnect(): void {
    const ICE_AUTO_RECONNECT_SECS = 3;
    let remaining = ICE_AUTO_RECONNECT_SECS;

    console.log(`ICE ${this.pc?.iceConnectionState}, auto-reconnecting in ${remaining}s...`);
    this.autoReconnectingCallback?.(remaining);

    this.iceAutoReconnectCountdown = setInterval(() => {
      remaining--;
      if (remaining <= 0) {
        this.cancelIceAutoReconnect();
        this.triggerAutoReconnect();
      } else {
        this.autoReconnectingCallback?.(remaining);
      }
    }, 1_000);

    // Also set the main timer as a safety net (fires at exactly 3s)
    this.iceDisconnectTimer = setTimeout(() => {
      this.cancelIceAutoReconnect();
      this.triggerAutoReconnect();
    }, ICE_AUTO_RECONNECT_SECS * 1_000);
  }

  /** Cancel any pending ICE auto-reconnect countdown and timers */
  private cancelIceAutoReconnect(): void {
    if (this.iceAutoReconnectCountdown) {
      clearInterval(this.iceAutoReconnectCountdown);
      this.iceAutoReconnectCountdown = null;
    }
    if (this.iceDisconnectTimer) {
      clearTimeout(this.iceDisconnectTimer);
      this.iceDisconnectTimer = null;
    }
  }

  /** Execute the auto-reconnect: notify UI with 0 remaining, then reconnect */
  private triggerAutoReconnect(): void {
    console.warn("ICE auto-reconnect: initiating full reconnect");
    this.autoReconnectingCallback?.(0);
    this.scheduleReconnect();
  }

  private async createAndSendOffer(): Promise<void> {
    if (!this.pc) return;

    // Only create a new offer on the first attempt. Retries re-send the
    // same SDP to avoid resetting the local ICE state mid-negotiation.
    if (!this.pendingOfferSdp) {
      this.pc.addTransceiver("video", { direction: "recvonly" });
      this.pc.addTransceiver("audio", { direction: "recvonly" });

      const offer = await this.pc.createOffer();

      // Prefer H.264 codec by reordering SDP
      if (offer.sdp) {
        offer.sdp = this.preferH264(offer.sdp);
      }

      await this.pc.setLocalDescription(offer);
      this.pendingOfferSdp = offer.sdp!;
    }

    this.sendSignal({
      type: "offer",
      sdp: this.pendingOfferSdp,
      session_id: this.sessionId,
    });

    // Retry offer if no answer received within 3 seconds.
    // The agent may not be connected yet when the browser first sends.
    this.scheduleOfferRetry();
  }

  private scheduleOfferRetry(): void {
    if (this.offerRetryTimer) {
      clearTimeout(this.offerRetryTimer);
    }

    const maxRetries = 5;
    if (this.offerRetryCount >= maxRetries) return;

    // First retry is fast (1s) since the agent may still be starting up.
    // Subsequent retries use 3s intervals.
    const delay = this.offerRetryCount === 0 ? 1_000 : 3_000;
    this.offerRetryTimer = setTimeout(() => {
      this.offerRetryTimer = null;
      // Only retry if we still don't have a remote description (no answer received)
      if (this.pc && !this.pc.remoteDescription) {
        this.offerRetryCount++;
        console.log(
          `No answer received, re-sending offer (attempt ${this.offerRetryCount}/${maxRetries})`,
        );
        this.createAndSendOffer();
      }
    }, delay);
  }

  private handleSignalMessage(msg: SignalMessage): void {
    switch (msg.type) {
      case "answer":
        if (msg.sdp && this.pc) {
          // Cancel offer retry - agent responded
          if (this.offerRetryTimer) {
            clearTimeout(this.offerRetryTimer);
            this.offerRetryTimer = null;
          }
          this.pendingOfferSdp = null;
          this.pc
            .setRemoteDescription({ type: "answer", sdp: msg.sdp })
            .then(() => console.log("Remote description set (answer)"))
            .catch((err) =>
              console.error("Failed to set remote description:", err),
            );
        }
        break;

      case "ice_candidate":
        if (msg.candidate && this.pc) {
          // webrtc-rs may send sdp_mid as "" (empty string); Chrome rejects
          // empty-string sdpMid because it doesn't match any mid. Convert to
          // null so Chrome falls back to sdpMLineIndex.
          this.pc
            .addIceCandidate(
              new RTCIceCandidate({
                candidate: msg.candidate,
                sdpMid: msg.sdp_mid || null,
                sdpMLineIndex: msg.sdp_mline_index,
              }),
            )
            .catch((err) => console.error("Failed to add ICE candidate:", err));
        }
        break;

      case "error":
        if (msg.message === "replaced") {
          console.log("Session taken over by another tab");
          this.intentionalDisconnect = true;
          this.cleanup();
          this.replacedCallback?.();
          return;
        }
        if (msg.message === "agent_exited") {
          console.error("Agent process exited unexpectedly");
          this.intentionalDisconnect = true;
          this.cleanup();
          this.agentExitedCallback?.();
          return;
        }
        console.error("Server signaling error:", msg.message);
        break;
    }
  }

  private sendSignal(msg: SignalMessage): void {
    if (this.ws?.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify(msg));
    }
  }

  /**
   * Reorder SDP to prefer H.264 codec.
   * Finds H.264 payload types in the video m-line and moves them to the front.
   */
  private preferH264(sdp: string): string {
    const lines = sdp.split("\r\n");
    const result: string[] = [];

    for (let i = 0; i < lines.length; i++) {
      const line = lines[i];

      if (line.startsWith("m=video")) {
        // Parse m=video line: m=video <port> <proto> <fmt list>
        const parts = line.split(" ");
        const header = parts.slice(0, 3);
        const payloadTypes = parts.slice(3);

        // Find H.264 payload types from rtpmap lines
        const h264PayloadTypes: string[] = [];
        for (const rtpLine of lines) {
          const match = rtpLine.match(/^a=rtpmap:(\d+)\s+H264\//i);
          if (match) {
            h264PayloadTypes.push(match[1]);
          }
        }

        // Move H.264 payload types to front
        const reordered = [
          ...h264PayloadTypes.filter((pt) => payloadTypes.includes(pt)),
          ...payloadTypes.filter((pt) => !h264PayloadTypes.includes(pt)),
        ];

        result.push([...header, ...reordered].join(" "));
      } else {
        result.push(line);
      }
    }

    return result.join("\r\n");
  }

  private scheduleReconnect(): void {
    if (this.intentionalDisconnect || this.reconnectTimer) return;

    if (this.reconnectAttempt >= MAX_RECONNECT_ATTEMPTS) {
      console.error(`Max reconnect attempts (${MAX_RECONNECT_ATTEMPTS}) reached`);
      this.reconnectFailedCallback?.();
      return;
    }

    // Exponential backoff with jitter to avoid thundering herd
    const baseDelay = Math.min(
      BASE_RECONNECT_DELAY_MS * Math.pow(2, this.reconnectAttempt),
      MAX_RECONNECT_DELAY_MS,
    );
    const jitter = Math.random() * baseDelay * 0.3;
    const delay = Math.round(baseDelay + jitter);
    this.reconnectAttempt++;

    console.log(
      `Reconnecting in ${delay}ms (attempt ${this.reconnectAttempt}/${MAX_RECONNECT_ATTEMPTS})...`,
    );
    this.reconnectingCallback?.(this.reconnectAttempt, MAX_RECONNECT_ATTEMPTS);

    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.establishConnection();
    }, delay);
  }

  private cleanup(): void {
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }

    if (this.offerRetryTimer) {
      clearTimeout(this.offerRetryTimer);
      this.offerRetryTimer = null;
    }
    this.offerRetryCount = 0;
    this.pendingOfferSdp = null;

    this.cancelIceAutoReconnect();

    if (this.dataChannel) {
      this.dataChannel.close();
      this.dataChannel = null;
    }

    if (this.pc) {
      this.pc.close();
      this.pc = null;
    }

    if (this.ws) {
      this.ws.close();
      this.ws = null;
    }
  }
}
