import { describe, it, expect, vi, beforeEach } from "vitest";
import { roundToEven, isSignificantResize } from "./resize";
import type { InputEvent } from "./connection";

// Use absolute path to ensure vite resolves the .ts source file, not the
// stale .js build artifact sitting next to it. Vite transforms .ts files
// through esbuild regardless of how they're referenced.
// @ts-expect-error -- absolute .ts path not recognized by tsc but works in vite
const { InputHandler } = await import("/home/frecar/code/beam/web/src/input.ts");

describe("roundToEven", () => {
  it("rounds odd numbers down to even", () => {
    expect(roundToEven(1921)).toBe(1920);
    expect(roundToEven(1081)).toBe(1080);
    expect(roundToEven(801)).toBe(800);
  });

  it("keeps even numbers unchanged", () => {
    expect(roundToEven(1920)).toBe(1920);
    expect(roundToEven(1080)).toBe(1080);
    expect(roundToEven(0)).toBe(0);
  });

  it("handles fractional values", () => {
    expect(roundToEven(1920.7)).toBe(1920);
    expect(roundToEven(1919.9)).toBe(1918);
    expect(roundToEven(1081.5)).toBe(1080);
  });

  it("handles small values", () => {
    expect(roundToEven(1)).toBe(0);
    expect(roundToEven(2)).toBe(2);
    expect(roundToEven(3)).toBe(2);
  });
});

describe("isSignificantResize", () => {
  it("returns false for first resize (old dimensions are 0)", () => {
    expect(isSignificantResize(0, 0, 1920, 1080)).toBe(false);
    expect(isSignificantResize(0, 1080, 1920, 1080)).toBe(false);
  });

  it("returns false for identical dimensions", () => {
    expect(isSignificantResize(1920, 1080, 1920, 1080)).toBe(false);
  });

  it("returns false for small changes (<10%)", () => {
    // 2px difference on 1920 = 0.1%
    expect(isSignificantResize(1920, 1080, 1922, 1080)).toBe(false);
    // 50px on 1080 = 4.6%
    expect(isSignificantResize(1920, 1080, 1920, 1030)).toBe(false);
    // Status bar: 28px on 1080 = 2.6%
    expect(isSignificantResize(1920, 1080, 1920, 1052)).toBe(false);
  });

  it("returns true for fullscreen-like changes (>10%)", () => {
    // 1512x800 → 1920x1200 (27% width, 50% height)
    expect(isSignificantResize(1512, 800, 1920, 1200)).toBe(true);
    // 1920x1080 → 1280x720 (33% width, 33% height)
    expect(isSignificantResize(1920, 1080, 1280, 720)).toBe(true);
  });

  it("detects significant width change with same height", () => {
    // 1920 → 2560 = 33% change
    expect(isSignificantResize(1920, 1080, 2560, 1080)).toBe(true);
  });

  it("detects significant height change with same width", () => {
    // 1080 → 1440 = 33% change
    expect(isSignificantResize(1920, 1080, 1920, 1440)).toBe(true);
  });

  it("handles edge case: exactly 10% change", () => {
    // 10% of 1000 = 100, so 1000 → 1100 = exactly 10%
    // > 0.1 is false for exactly 0.1
    expect(isSignificantResize(1000, 1000, 1100, 1000)).toBe(false);
    // 10.1% should be true
    expect(isSignificantResize(1000, 1000, 1101, 1000)).toBe(true);
  });
});

/**
 * Scroll multiplier tests for InputHandler.
 *
 * Strategy: create a minimal mock HTMLElement that captures event listeners,
 * construct an InputHandler, call enable(), then invoke the captured wheel
 * handler directly with mock WheelEvent objects. Verify the sendInput callback
 * receives correctly scaled scroll deltas.
 */
describe("InputHandler scroll multiplier", () => {
  let sent: InputEvent[];
  let handler: InstanceType<typeof InputHandler>;
  let wheelHandler: (e: WheelEvent) => void;

  /** Create a minimal mock HTMLElement with just enough API surface for InputHandler */
  function createMockTarget(): HTMLElement {
    const listeners: Record<string, Function> = {};
    return {
      addEventListener: (type: string, fn: Function, _opts?: any) => {
        listeners[type] = fn;
      },
      removeEventListener: () => {},
      querySelector: () => null,
      getBoundingClientRect: () => ({ left: 0, top: 0, width: 1920, height: 1080 }),
      style: {},
      // Expose captured listeners for test access
      __listeners: listeners,
    } as unknown as HTMLElement;
  }

  /** Create a mock WheelEvent with the given deltas and mode */
  function createWheelEvent(deltaX: number, deltaY: number, deltaMode = 0): WheelEvent {
    return {
      deltaX,
      deltaY,
      deltaMode,
      preventDefault: () => {},
    } as unknown as WheelEvent;
  }

  beforeEach(() => {
    // Stub document methods that InputHandler.enable() calls
    vi.stubGlobal("document", {
      addEventListener: () => {},
      removeEventListener: () => {},
      getElementById: () => null,
      pointerLockElement: null,
      fullscreenElement: null,
    });

    // Stub ResizeObserver (used by enable())
    vi.stubGlobal("ResizeObserver", class {
      observe() {}
      disconnect() {}
    });

    sent = [];
    const target = createMockTarget();
    handler = new InputHandler(target, (event: InputEvent) => {
      sent.push(event);
    });
    handler.enable();

    // Extract the wheel handler that InputHandler registered
    wheelHandler = (target as any).__listeners["wheel"];
  });

  it("has a setScrollMultiplier method", () => {
    expect(typeof handler.setScrollMultiplier).toBe("function");
  });

  it("default scroll multiplier is 1.0 (deltas pass through unchanged)", () => {
    wheelHandler(createWheelEvent(0, 100));

    expect(sent).toHaveLength(1);
    expect(sent[0]).toEqual({ t: "s", dx: 0, dy: 100 });
  });

  it("scroll multiplier 2.0x doubles wheel deltas", () => {
    handler.setScrollMultiplier(2.0);
    wheelHandler(createWheelEvent(10, 50));

    expect(sent).toHaveLength(1);
    expect(sent[0]).toEqual({ t: "s", dx: 20, dy: 100 });
  });

  it("scroll multiplier 0.5x halves wheel deltas", () => {
    handler.setScrollMultiplier(0.5);
    wheelHandler(createWheelEvent(40, 200));

    expect(sent).toHaveLength(1);
    expect(sent[0]).toEqual({ t: "s", dx: 20, dy: 100 });
  });

  it("scroll multiplier 3.0x triples wheel deltas", () => {
    handler.setScrollMultiplier(3.0);
    wheelHandler(createWheelEvent(10, -30));

    expect(sent).toHaveLength(1);
    expect(sent[0]).toEqual({ t: "s", dx: 30, dy: -90 });
  });

  it("scroll multiplier applies after deltaMode line scaling", () => {
    // deltaMode 1 = DOM_DELTA_LINE, multiplied by 30 before scroll multiplier
    handler.setScrollMultiplier(2.0);
    wheelHandler(createWheelEvent(0, 3, 1));

    // 3 lines * 30 px/line = 90, then * 2.0 multiplier = 180
    expect(sent).toHaveLength(1);
    expect(sent[0]).toEqual({ t: "s", dx: 0, dy: 180 });
  });

  it("scroll multiplier applies after deltaMode page scaling", () => {
    // deltaMode 2 = DOM_DELTA_PAGE, multiplied by 300 before scroll multiplier
    handler.setScrollMultiplier(2.0);
    wheelHandler(createWheelEvent(1, 1, 2));

    // 1 page * 300 px/page = 300, then * 2.0 multiplier = 600
    expect(sent).toHaveLength(1);
    expect(sent[0]).toEqual({ t: "s", dx: 600, dy: 600 });
  });
});
