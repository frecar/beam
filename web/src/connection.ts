/**
 * WebSocket-only connection to the Beam server.
 *
 * Video (H.264 Annex B) and audio (Opus) frames arrive as binary WebSocket
 * messages with a 24-byte header. Input events and signaling messages are
 * sent/received as JSON text messages.
 *
 * Binary frame header format (24 bytes, little-endian):
 *   [0..4]   magic: 0x56414542 ("BEAV" in LE)
 *   [4]      version: 1
 *   [5]      flags: bit 0 = keyframe, bit 1 = audio
 *   [6..8]   width (u16 LE)
 *   [8..10]  height (u16 LE)
 *   [10..12] reserved (u16, always 0)
 *   [12..20] timestamp_us (u64 LE) -- microseconds since capture start
 *   [20..24] payload_length (u32 LE)
 *   [24..]   payload
 */

const FRAME_HEADER_SIZE = 24;
const FRAME_MAGIC = 0x56414542; // "BEAV" in little-endian

/**
 * Input events sent over the WebSocket as JSON text.
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
  | { t: "fd"; id: string }
  | { t: "fdr"; path: string }
  | { t: "fds"; id: string; name: string; size: number }
  | { t: "fdc"; id: string; data: string }
  | { t: "fdd"; id: string }
  | { t: "fde"; id: string; error: string };

/** Signaling/control messages received as JSON text from the server */
type ServerMessage =
  | { type: "session_ready" }
  | { type: "error"; message: string };

type VoidCallback = () => void;
type VideoFrameCallback = (
  flags: number,
  width: number,
  height: number,
  timestampUs: bigint,
  payload: Uint8Array,
) => void;
type AudioFrameCallback = (timestampUs: bigint, payload: Uint8Array) => void;

const MAX_RECONNECT_DELAY_MS = 30_000;
const BASE_RECONNECT_DELAY_MS = 1_000;
const MAX_RECONNECT_ATTEMPTS = 10;

/**
 * Manages WebSocket connection to the Beam server.
 * Binary messages carry video/audio frames; text messages carry input events
 * and signaling.
 */
export class BeamConnection {
  private sessionId: string;
  private token: string;
  private ws: WebSocket | null = null;
  private reconnectAttempt = 0;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private intentionalDisconnect = false;

  // Callbacks
  private videoFrameCallback: VideoFrameCallback | null = null;
  private audioFrameCallback: AudioFrameCallback | null = null;
  private connectedCallback: VoidCallback | null = null;
  private disconnectCallback: VoidCallback | null = null;
  private reconnectingCallback: ((attempt: number, maxAttempts: number) => void) | null = null;
  private reconnectFailedCallback: VoidCallback | null = null;
  private agentMessageCallback: ((msg: InputEvent) => void) | null = null;
  private replacedCallback: VoidCallback | null = null;
  private agentExitedCallback: VoidCallback | null = null;

  constructor(sessionId: string, token: string) {
    this.sessionId = sessionId;
    this.token = token;
  }

  /** Register callback for decoded video frames */
  onVideoFrame(callback: VideoFrameCallback): void {
    this.videoFrameCallback = callback;
  }

  /** Register callback for decoded audio frames */
  onAudioFrame(callback: AudioFrameCallback): void {
    this.audioFrameCallback = callback;
  }

  /** Register callback for when the WebSocket connection opens */
  onConnected(callback: VoidCallback): void {
    this.connectedCallback = callback;
  }

  /** Register callback for when the connection is lost */
  onDisconnect(callback: VoidCallback): void {
    this.disconnectCallback = callback;
  }

  /** Register callback for reconnection attempts */
  onReconnecting(callback: (attempt: number, maxAttempts: number) => void): void {
    this.reconnectingCallback = callback;
  }

  /** Register callback for when all reconnection attempts exhausted */
  onReconnectFailed(callback: VoidCallback): void {
    this.reconnectFailedCallback = callback;
  }

  /** Register callback for agent-to-browser text messages (cursor, file download, clipboard) */
  onAgentMessage(callback: (msg: InputEvent) => void): void {
    this.agentMessageCallback = callback;
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

  /** Establish WebSocket connection */
  async connect(): Promise<void> {
    this.intentionalDisconnect = false;
    this.reconnectAttempt = 0;
    await this.establishConnection();
  }

  /** Cleanly tear down the connection */
  disconnect(): void {
    this.intentionalDisconnect = true;
    this.cleanup();
  }

  /** Send an input event as JSON text over WebSocket */
  sendInput(event: InputEvent): void {
    if (this.ws?.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify(event));
    }
  }

  private async establishConnection(): Promise<void> {
    this.cleanup();

    const wsProtocol = location.protocol === "https:" ? "wss:" : "ws:";
    const wsUrl = `${wsProtocol}//${location.host}/api/sessions/${this.sessionId}/ws?token=${encodeURIComponent(this.token)}`;

    this.ws = new WebSocket(wsUrl);
    this.ws.binaryType = "arraybuffer";

    let wsOpened = false;

    this.ws.onopen = () => {
      wsOpened = true;
      this.reconnectAttempt = 0;
      this.connectedCallback?.();
    };

    this.ws.onmessage = (event: MessageEvent) => {
      if (event.data instanceof ArrayBuffer) {
        this.handleBinaryMessage(event.data);
      } else if (typeof event.data === "string") {
        this.handleTextMessage(event.data);
      }
    };

    this.ws.onclose = (event: CloseEvent) => {
      console.log(
        `WebSocket closed: code=${event.code} reason=${event.reason} clean=${event.wasClean} intentional=${this.intentionalDisconnect}`,
      );
      setTimeout(() => {
        if (!this.intentionalDisconnect) {
          if (!wsOpened && event.code === 1006) {
            console.error("WebSocket rejected (likely auth failure), not retrying");
            this.reconnectFailedCallback?.();
            return;
          }
          this.scheduleReconnect();
        }
        this.disconnectCallback?.();
      }, 50);
    };

    this.ws.onerror = () => {
      // onclose fires after onerror; reconnect handled there
    };
  }

  /** Parse a 24-byte binary frame header and dispatch to video/audio callback */
  private handleBinaryMessage(data: ArrayBuffer): void {
    if (data.byteLength < FRAME_HEADER_SIZE) {
      console.warn("Binary message too short for frame header:", data.byteLength);
      return;
    }

    const view = new DataView(data);
    const magic = view.getUint32(0, true);
    if (magic !== FRAME_MAGIC) {
      console.warn("Invalid frame magic:", magic.toString(16));
      return;
    }

    const flags = view.getUint8(5);
    const width = view.getUint16(6, true);
    const height = view.getUint16(8, true);
    const timestampUs = view.getBigUint64(12, true);
    const payloadLength = view.getUint32(20, true);

    const expectedSize = FRAME_HEADER_SIZE + payloadLength;
    if (data.byteLength < expectedSize) {
      console.warn(`Frame truncated: expected ${expectedSize}, got ${data.byteLength}`);
      return;
    }

    const payload = new Uint8Array(data, FRAME_HEADER_SIZE, payloadLength);
    const isAudio = (flags & 0x02) !== 0;

    if (isAudio) {
      this.audioFrameCallback?.(timestampUs, payload);
    } else {
      this.videoFrameCallback?.(flags, width, height, timestampUs, payload);
    }
  }

  /** Handle incoming JSON text messages (signaling + agent messages) */
  private handleTextMessage(data: string): void {
    let msg: any;
    try {
      msg = JSON.parse(data);
    } catch {
      console.warn("Failed to parse text message:", data);
      return;
    }

    // Server signaling messages
    if (msg.type === "error") {
      const serverMsg = msg as ServerMessage & { type: "error" };
      if (serverMsg.message === "replaced") {
        console.log("Session taken over by another tab");
        this.intentionalDisconnect = true;
        this.cleanup();
        this.replacedCallback?.();
        return;
      }
      if (serverMsg.message === "agent_exited") {
        console.error("Agent process exited unexpectedly");
        this.intentionalDisconnect = true;
        this.cleanup();
        this.agentExitedCallback?.();
        return;
      }
      console.error("Server error:", serverMsg.message);
      return;
    }

    // Agent-to-browser messages (clipboard, cursor, file download events)
    // These have a "t" field matching the InputEvent discriminator
    if (msg.t) {
      this.agentMessageCallback?.(msg as InputEvent);
    }
  }

  private scheduleReconnect(): void {
    if (this.intentionalDisconnect || this.reconnectTimer) return;

    if (this.reconnectAttempt >= MAX_RECONNECT_ATTEMPTS) {
      console.error(`Max reconnect attempts (${MAX_RECONNECT_ATTEMPTS}) reached`);
      this.reconnectFailedCallback?.();
      return;
    }

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

    if (this.ws) {
      this.ws.close();
      this.ws = null;
    }
  }
}
