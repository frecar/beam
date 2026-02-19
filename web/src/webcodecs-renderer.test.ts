import { describe, it, expect, vi, beforeEach, afterEach } from "vitest";
import { WebCodecsRenderer } from "./webcodecs-renderer";

// =========================================================================
// Mock browser APIs
// =========================================================================

let decodedVideoChunks: any[];
let decoderConfigureCalls: any[];
let decoderCloseCalls: number;
let outputCallback: ((frame: any) => void) | null;

class MockVideoDecoder {
  state = "unconfigured";

  constructor({ output }: any) {
    outputCallback = output;
  }

  configure(_config: any) {
    this.state = "configured";
    decoderConfigureCalls.push(_config);
  }

  decode(chunk: any) {
    decodedVideoChunks.push(chunk);
    // Simulate synchronous output callback with a mock VideoFrame
    outputCallback?.({
      close: vi.fn(),
    });
  }

  close() {
    this.state = "closed";
    decoderCloseCalls++;
  }
}

let decodedAudioChunks: any[];
let audioDecoderConfigured: boolean;

class MockAudioDecoder {
  state = "unconfigured";
  private audioOutputCallback: ((data: any) => void) | null = null;

  constructor({ output }: any) {
    this.audioOutputCallback = output;
    audioDecoderConfigured = false;
  }

  configure(_config: any) {
    this.state = "configured";
    audioDecoderConfigured = true;
  }

  decode(chunk: any) {
    decodedAudioChunks.push(chunk);
    // Simulate output with mock AudioData
    this.audioOutputCallback?.({
      numberOfFrames: 480,
      numberOfChannels: 2,
      sampleRate: 48000,
      copyTo: vi.fn(),
      close: vi.fn(),
    });
  }

  close() {
    this.state = "closed";
  }
}

class MockEncodedVideoChunk {
  type: string;
  timestamp: number;
  data: any;
  constructor({ type, timestamp, data }: any) {
    this.type = type;
    this.timestamp = timestamp;
    this.data = data;
  }
}

class MockEncodedAudioChunk {
  type: string;
  timestamp: number;
  data: any;
  constructor({ type, timestamp, data }: any) {
    this.type = type;
    this.timestamp = timestamp;
    this.data = data;
  }
}

function createMockCanvas(): HTMLCanvasElement {
  return {
    width: 0,
    height: 0,
    getContext: () => ({
      drawImage: vi.fn(),
    }),
  } as unknown as HTMLCanvasElement;
}

function createMockContainer(): HTMLElement {
  return {
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
    requestFullscreen: vi.fn(),
  } as unknown as HTMLElement;
}

// =========================================================================
// Tests
// =========================================================================

describe("WebCodecsRenderer", () => {
  let renderer: WebCodecsRenderer;
  let canvas: HTMLCanvasElement;
  let container: HTMLElement;

  beforeEach(() => {
    decodedVideoChunks = [];
    decoderConfigureCalls = [];
    decoderCloseCalls = 0;
    decodedAudioChunks = [];
    audioDecoderConfigured = false;
    outputCallback = null;

    vi.stubGlobal("VideoDecoder", MockVideoDecoder);
    vi.stubGlobal("AudioDecoder", MockAudioDecoder);
    vi.stubGlobal("EncodedVideoChunk", MockEncodedVideoChunk);
    vi.stubGlobal("EncodedAudioChunk", MockEncodedAudioChunk);
    vi.stubGlobal("AudioContext", class {
      state = "running";
      currentTime = 0;
      sampleRate = 48000;
      createBuffer(_channels: number, frames: number, rate: number) {
        return {
          duration: frames / rate,
          getChannelData: () => new Float32Array(frames),
        };
      }
      createBufferSource() {
        return {
          buffer: null,
          connect: vi.fn(),
          start: vi.fn(),
        };
      }
      get destination() { return {}; }
      suspend() { this.state = "suspended"; }
      resume() { this.state = "running"; }
      close() {}
    });
    vi.stubGlobal("localStorage", {
      getItem: () => null,
      setItem: vi.fn(),
    });
    vi.stubGlobal("performance", {
      now: () => 0,
    });

    canvas = createMockCanvas();
    container = createMockContainer();
    renderer = new WebCodecsRenderer(canvas, container);
  });

  afterEach(() => {
    renderer.destroy();
    vi.unstubAllGlobals();
  });

  it("drops P-frames before first keyframe", () => {
    const payload = new Uint8Array([1, 2, 3]);
    renderer.feedVideoFrame(0x00, 1920, 1080, 0n, payload); // P-frame

    // configureDecoder is called (resolution triggers it), but decode should not happen
    // because needsKeyframe=true and this is not a keyframe
    expect(decodedVideoChunks).toHaveLength(0);
  });

  it("accepts keyframe when needsKeyframe=true", () => {
    const payload = new Uint8Array([1, 2, 3]);
    renderer.feedVideoFrame(0x01, 1920, 1080, 0n, payload); // keyframe

    expect(decodedVideoChunks).toHaveLength(1);
    expect(decodedVideoChunks[0].type).toBe("key");
  });

  it("accepts P-frame after keyframe", () => {
    const payload = new Uint8Array([1, 2, 3]);
    renderer.feedVideoFrame(0x01, 1920, 1080, 0n, payload); // keyframe
    renderer.feedVideoFrame(0x00, 1920, 1080, 1000n, payload); // P-frame

    expect(decodedVideoChunks).toHaveLength(2);
    expect(decodedVideoChunks[0].type).toBe("key");
    expect(decodedVideoChunks[1].type).toBe("delta");
  });

  it("reconfigures decoder on resolution change", () => {
    const payload = new Uint8Array([1, 2, 3]);
    renderer.feedVideoFrame(0x01, 1920, 1080, 0n, payload);

    expect(decoderConfigureCalls).toHaveLength(1);
    const closesBefore = decoderCloseCalls;

    // Different resolution triggers reconfigure
    renderer.feedVideoFrame(0x01, 1280, 720, 1000n, payload);

    expect(decoderConfigureCalls).toHaveLength(2);
    expect(decoderCloseCalls).toBeGreaterThan(closesBefore);
  });

  it("needsKeyframe resets on resolution change", () => {
    const payload = new Uint8Array([1, 2, 3]);
    // Feed keyframe at 1920x1080
    renderer.feedVideoFrame(0x01, 1920, 1080, 0n, payload);
    expect(decodedVideoChunks).toHaveLength(1);

    // Feed P-frame at NEW resolution — should be dropped (needs new keyframe)
    renderer.feedVideoFrame(0x00, 1280, 720, 1000n, payload);
    expect(decodedVideoChunks).toHaveLength(1); // still 1, P-frame dropped
  });

  it("audio muted by default", () => {
    const payload = new Uint8Array([1, 2, 3]);
    renderer.feedAudioFrame(0n, payload);

    expect(decodedAudioChunks).toHaveLength(0);
    expect(audioDecoderConfigured).toBe(false);
  });

  it("audio plays when unmuted", () => {
    renderer.setAudioMuted(false);
    const payload = new Uint8Array([1, 2, 3]);
    renderer.feedAudioFrame(0n, payload);

    expect(decodedAudioChunks).toHaveLength(1);
  });

  it("first frame callback fires once", () => {
    let callCount = 0;
    renderer.onFirstFrame(() => callCount++);

    const payload = new Uint8Array([1, 2, 3]);
    renderer.feedVideoFrame(0x01, 1920, 1080, 0n, payload);
    renderer.feedVideoFrame(0x01, 1920, 1080, 1000n, payload);

    expect(callCount).toBe(1);
  });

  it("destroy cleans up decoders", () => {
    const payload = new Uint8Array([1, 2, 3]);
    renderer.feedVideoFrame(0x01, 1920, 1080, 0n, payload);
    renderer.setAudioMuted(false);
    renderer.feedAudioFrame(0n, payload);

    const closesBefore = decoderCloseCalls;
    renderer.destroy();
    expect(decoderCloseCalls).toBeGreaterThan(closesBefore);
  });
});
