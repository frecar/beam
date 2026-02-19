import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import {
  parseFrameHeader,
  FRAME_HEADER_SIZE,
  FRAME_MAGIC,
  BeamConnection,
} from "./connection";

/**
 * Build a binary frame buffer matching the Rust VideoFrameHeader::serialize() layout.
 * 24-byte header (little-endian) + payload.
 */
function buildFrameBuffer(
  flags: number,
  width: number,
  height: number,
  timestampUs: bigint,
  payload: Uint8Array,
): ArrayBuffer {
  const buf = new ArrayBuffer(FRAME_HEADER_SIZE + payload.byteLength);
  const view = new DataView(buf);
  view.setUint32(0, FRAME_MAGIC, true); // magic
  view.setUint8(4, 1); // version
  view.setUint8(5, flags); // flags
  view.setUint16(6, width, true); // width
  view.setUint16(8, height, true); // height
  view.setUint16(10, 0, true); // reserved
  view.setBigUint64(12, timestampUs, true); // timestamp_us
  view.setUint32(20, payload.byteLength, true); // payload_length
  new Uint8Array(buf, FRAME_HEADER_SIZE).set(payload);
  return buf;
}

// =========================================================================
// parseFrameHeader — cross-language contract tests
// =========================================================================

describe("parseFrameHeader", () => {
  it("parses valid video frame", () => {
    const payload = new Uint8Array([0xde, 0xad, 0xbe, 0xef]);
    const buf = buildFrameBuffer(0x01, 1920, 1080, 123456n, payload);
    const result = parseFrameHeader(buf);

    expect(result).not.toBeNull();
    expect(result!.header.flags).toBe(0x01);
    expect(result!.header.width).toBe(1920);
    expect(result!.header.height).toBe(1080);
    expect(result!.header.timestampUs).toBe(123456n);
    expect(result!.header.payloadLength).toBe(4);
    expect(Array.from(result!.payload)).toEqual([0xde, 0xad, 0xbe, 0xef]);
  });

  it("parses valid audio frame", () => {
    const payload = new Uint8Array([1, 2, 3]);
    const buf = buildFrameBuffer(0x02, 0, 0, 999999n, payload);
    const result = parseFrameHeader(buf);

    expect(result).not.toBeNull();
    expect(result!.header.flags & 0x02).toBe(0x02);
    expect(result!.header.timestampUs).toBe(999999n);
    expect(result!.header.payloadLength).toBe(3);
  });

  it("rejects too-short message", () => {
    const buf = new ArrayBuffer(20);
    expect(parseFrameHeader(buf)).toBeNull();
  });

  it("rejects bad magic", () => {
    const buf = new ArrayBuffer(FRAME_HEADER_SIZE);
    const view = new DataView(buf);
    view.setUint32(0, 0xdeadbeef, true);
    expect(parseFrameHeader(buf)).toBeNull();
  });

  it("rejects truncated payload", () => {
    // Header says 100 bytes payload, but only 50 present
    const buf = new ArrayBuffer(FRAME_HEADER_SIZE + 50);
    const view = new DataView(buf);
    view.setUint32(0, FRAME_MAGIC, true);
    view.setUint8(4, 1);
    view.setUint32(20, 100, true); // payload_length = 100
    expect(parseFrameHeader(buf)).toBeNull();
  });

  it("parses keyframe flag correctly", () => {
    const payload = new Uint8Array([0xff]);
    const buf = buildFrameBuffer(0x01, 640, 480, 0n, payload);
    const result = parseFrameHeader(buf);

    expect(result).not.toBeNull();
    expect(result!.header.flags & 0x01).toBe(0x01);
  });

  it("handles zero-length payload", () => {
    const buf = buildFrameBuffer(0x00, 1920, 1080, 42n, new Uint8Array(0));
    const result = parseFrameHeader(buf);

    expect(result).not.toBeNull();
    expect(result!.header.payloadLength).toBe(0);
    expect(result!.payload.byteLength).toBe(0);
  });

  it("header constants match Rust protocol", () => {
    expect(FRAME_HEADER_SIZE).toBe(24);
    expect(FRAME_MAGIC).toBe(0x56414542);
    // Verify magic spells "BEAV" in LE
    const magicBytes = new Uint8Array(4);
    new DataView(magicBytes.buffer).setUint32(0, FRAME_MAGIC, true);
    expect(String.fromCharCode(...magicBytes)).toBe("BEAV");
  });
});

// =========================================================================
// BeamConnection reconnect logic tests
// =========================================================================

/** Captured mock WebSocket instances for test inspection */
let mockWsInstances: MockWebSocket[];

class MockWebSocket {
  binaryType = "";
  readyState = 0; // CONNECTING
  onopen: ((ev: any) => void) | null = null;
  onclose: ((ev: any) => void) | null = null;
  onmessage: ((ev: any) => void) | null = null;
  onerror: ((ev: any) => void) | null = null;

  static CONNECTING = 0;
  static OPEN = 1;
  static CLOSING = 2;
  static CLOSED = 3;

  constructor(_url: string) {
    mockWsInstances.push(this);
  }

  close() {
    this.readyState = MockWebSocket.CLOSED;
  }

  send(_data: any) {}

  /** Test helper: simulate the WS opening */
  simulateOpen() {
    this.readyState = MockWebSocket.OPEN;
    this.onopen?.({});
  }

  /** Test helper: simulate the WS closing */
  simulateClose(code = 1006, reason = "", wasClean = false) {
    this.readyState = MockWebSocket.CLOSED;
    this.onclose?.({ code, reason, wasClean });
  }

  /** Test helper: simulate a text message */
  simulateMessage(data: string) {
    this.onmessage?.({ data });
  }
}

describe("BeamConnection reconnect logic", () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.spyOn(Math, "random").mockReturnValue(0); // zero jitter
    mockWsInstances = [];

    vi.stubGlobal("WebSocket", MockWebSocket);
    vi.stubGlobal("location", { protocol: "https:", host: "localhost:8444" });
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.restoreAllMocks();
    vi.unstubAllGlobals();
  });

  it("exponential backoff doubles delay", async () => {
    const conn = new BeamConnection("test-session", "test-token");
    const reconnectingCalls: number[] = [];
    conn.onReconnecting((attempt) => reconnectingCalls.push(attempt));

    await conn.connect();
    const ws = mockWsInstances[0];
    ws.simulateOpen();

    // First disconnect: triggers 50ms onclose delay, then 1000ms backoff (jitter=0)
    ws.simulateClose(1000);
    await vi.advanceTimersByTimeAsync(50); // onclose setTimeout
    expect(reconnectingCalls).toEqual([1]);

    await vi.advanceTimersByTimeAsync(1000); // 1st reconnect fires
    const ws2 = mockWsInstances[1];
    ws2.simulateOpen();

    // Second disconnect: 2000ms backoff
    ws2.simulateClose(1000);
    await vi.advanceTimersByTimeAsync(50);
    expect(reconnectingCalls).toEqual([1, 1]); // attempt resets to 0 on success, so 1 again

    await vi.advanceTimersByTimeAsync(1000);
    const ws3 = mockWsInstances[2];
    // Don't open ws3 — let it close without opening to skip reset
    ws3.simulateClose(1000);
    await vi.advanceTimersByTimeAsync(50);
    // Now attempt=2, so delay should be 2000ms
    expect(reconnectingCalls).toEqual([1, 1, 2]);
  });

  it("max reconnect attempts exhausted calls reconnectFailedCallback", async () => {
    const conn = new BeamConnection("test-session", "test-token");
    let failedCalled = false;
    conn.onReconnectFailed(() => { failedCalled = true; });

    await conn.connect();
    const ws0 = mockWsInstances[0];
    ws0.simulateOpen();

    // scheduleReconnect increments reconnectAttempt before scheduling,
    // and checks >= MAX at entry. Need 10 increments (to reach 10), then
    // one more close to trigger the >= 10 check → 11 total close cycles.
    for (let i = 0; i < 11; i++) {
      const ws = mockWsInstances[mockWsInstances.length - 1];
      ws.simulateClose(1000);
      await vi.advanceTimersByTimeAsync(50); // onclose delay

      if (!failedCalled) {
        // Advance past the backoff to trigger next connection attempt
        await vi.advanceTimersByTimeAsync(60_000);
      }
    }

    expect(failedCalled).toBe(true);
  });

  it("intentional disconnect does not reconnect", async () => {
    const conn = new BeamConnection("test-session", "test-token");
    let reconnectingCalled = false;
    conn.onReconnecting(() => { reconnectingCalled = true; });

    await conn.connect();
    const ws = mockWsInstances[0];
    ws.simulateOpen();

    conn.disconnect();

    await vi.advanceTimersByTimeAsync(5000);
    expect(reconnectingCalled).toBe(false);
  });

  it("auth failure (1006 before open) does not retry", async () => {
    const conn = new BeamConnection("test-session", "test-token");
    let failedCalled = false;
    let reconnectingCalled = false;
    conn.onReconnectFailed(() => { failedCalled = true; });
    conn.onReconnecting(() => { reconnectingCalled = true; });

    await conn.connect();
    const ws = mockWsInstances[0];
    // Close with 1006 BEFORE onopen fires
    ws.simulateClose(1006);
    await vi.advanceTimersByTimeAsync(50);

    expect(failedCalled).toBe(true);
    expect(reconnectingCalled).toBe(false);
  });

  it("replaced message stops reconnection", async () => {
    const conn = new BeamConnection("test-session", "test-token");
    let replacedCalled = false;
    let reconnectingCalled = false;
    conn.onReplaced(() => { replacedCalled = true; });
    conn.onReconnecting(() => { reconnectingCalled = true; });

    await conn.connect();
    const ws = mockWsInstances[0];
    ws.simulateOpen();

    // Server sends "replaced" error
    ws.simulateMessage(JSON.stringify({ type: "error", message: "replaced" }));

    await vi.advanceTimersByTimeAsync(5000);
    expect(replacedCalled).toBe(true);
    expect(reconnectingCalled).toBe(false);
  });

  it("reconnect counter resets on successful connection", async () => {
    const conn = new BeamConnection("test-session", "test-token");
    const reconnectAttempts: number[] = [];
    conn.onReconnecting((attempt) => reconnectAttempts.push(attempt));

    await conn.connect();
    const ws0 = mockWsInstances[0];
    ws0.simulateOpen();

    // Disconnect + reconnect cycle
    ws0.simulateClose(1000);
    await vi.advanceTimersByTimeAsync(50);
    expect(reconnectAttempts).toEqual([1]);

    await vi.advanceTimersByTimeAsync(1000);
    const ws1 = mockWsInstances[1];
    ws1.simulateOpen(); // This resets the counter

    // Disconnect again — should start from attempt 1, not 2
    ws1.simulateClose(1000);
    await vi.advanceTimersByTimeAsync(50);
    expect(reconnectAttempts).toEqual([1, 1]);
  });
});
