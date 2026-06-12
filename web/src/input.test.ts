import { beforeEach, describe, expect, it, vi } from 'vitest';

import type { InputEvent } from './connection';
import { InputHandler } from './input';
import { isSignificantResize, roundToEven } from './resize';

describe('roundToEven', () => {
  it('rounds odd numbers down to even', () => {
    expect(roundToEven(1921)).toBe(1920);
    expect(roundToEven(1081)).toBe(1080);
    expect(roundToEven(801)).toBe(800);
  });

  it('keeps even numbers unchanged', () => {
    expect(roundToEven(1920)).toBe(1920);
    expect(roundToEven(1080)).toBe(1080);
    expect(roundToEven(0)).toBe(0);
  });

  it('handles fractional values', () => {
    expect(roundToEven(1920.7)).toBe(1920);
    expect(roundToEven(1919.9)).toBe(1918);
    expect(roundToEven(1081.5)).toBe(1080);
  });

  it('handles small values', () => {
    expect(roundToEven(1)).toBe(0);
    expect(roundToEven(2)).toBe(2);
    expect(roundToEven(3)).toBe(2);
  });
});

describe('isSignificantResize', () => {
  it('returns false for first resize (old dimensions are 0)', () => {
    expect(isSignificantResize(0, 0, 1920, 1080)).toBe(false);
    expect(isSignificantResize(0, 1080, 1920, 1080)).toBe(false);
  });

  it('returns false for identical dimensions', () => {
    expect(isSignificantResize(1920, 1080, 1920, 1080)).toBe(false);
  });

  it('returns false for small changes (<10%)', () => {
    // 2px difference on 1920 = 0.1%
    expect(isSignificantResize(1920, 1080, 1922, 1080)).toBe(false);
    // 50px on 1080 = 4.6%
    expect(isSignificantResize(1920, 1080, 1920, 1030)).toBe(false);
    // Status bar: 28px on 1080 = 2.6%
    expect(isSignificantResize(1920, 1080, 1920, 1052)).toBe(false);
  });

  it('returns true for fullscreen-like changes (>10%)', () => {
    // 1512x800 → 1920x1200 (27% width, 50% height)
    expect(isSignificantResize(1512, 800, 1920, 1200)).toBe(true);
    // 1920x1080 → 1280x720 (33% width, 33% height)
    expect(isSignificantResize(1920, 1080, 1280, 720)).toBe(true);
  });

  it('detects significant width change with same height', () => {
    // 1920 → 2560 = 33% change
    expect(isSignificantResize(1920, 1080, 2560, 1080)).toBe(true);
  });

  it('detects significant height change with same width', () => {
    // 1080 → 1440 = 33% change
    expect(isSignificantResize(1920, 1080, 1920, 1440)).toBe(true);
  });

  it('handles edge case: exactly 10% change', () => {
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
describe('InputHandler scroll multiplier', () => {
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
    vi.stubGlobal('document', {
      addEventListener: () => {},
      removeEventListener: () => {},
      getElementById: () => null,
      pointerLockElement: null,
      fullscreenElement: null,
    });

    // Stub ResizeObserver (used by enable())
    vi.stubGlobal(
      'ResizeObserver',
      class {
        observe() {}
        disconnect() {}
      }
    );

    sent = [];
    const target = createMockTarget();
    handler = new InputHandler(target, (event: InputEvent) => {
      sent.push(event);
    });
    handler.enable();

    // Extract the wheel handler that InputHandler registered
    wheelHandler = (target as any).__listeners['wheel'];
  });

  it('has a setScrollMultiplier method', () => {
    expect(typeof handler.setScrollMultiplier).toBe('function');
  });

  it('default scroll multiplier is 1.0 (deltas pass through unchanged)', () => {
    wheelHandler(createWheelEvent(0, 100));

    expect(sent).toHaveLength(1);
    expect(sent[0]).toEqual({ t: 's', dx: 0, dy: 100 });
  });

  it('scroll multiplier 2.0x doubles wheel deltas', () => {
    handler.setScrollMultiplier(2.0);
    wheelHandler(createWheelEvent(10, 50));

    expect(sent).toHaveLength(1);
    expect(sent[0]).toEqual({ t: 's', dx: 20, dy: 100 });
  });

  it('scroll multiplier 0.5x halves wheel deltas', () => {
    handler.setScrollMultiplier(0.5);
    wheelHandler(createWheelEvent(40, 200));

    expect(sent).toHaveLength(1);
    expect(sent[0]).toEqual({ t: 's', dx: 20, dy: 100 });
  });

  it('scroll multiplier 3.0x triples wheel deltas', () => {
    handler.setScrollMultiplier(3.0);
    wheelHandler(createWheelEvent(10, -30));

    expect(sent).toHaveLength(1);
    expect(sent[0]).toEqual({ t: 's', dx: 30, dy: -90 });
  });

  it('scroll multiplier applies after deltaMode line scaling', () => {
    // deltaMode 1 = DOM_DELTA_LINE, multiplied by 30 before scroll multiplier
    handler.setScrollMultiplier(2.0);
    wheelHandler(createWheelEvent(0, 3, 1));

    // 3 lines * 30 px/line = 90, then * 2.0 multiplier = 180
    expect(sent).toHaveLength(1);
    expect(sent[0]).toEqual({ t: 's', dx: 0, dy: 180 });
  });

  it('scroll multiplier applies after deltaMode page scaling', () => {
    // deltaMode 2 = DOM_DELTA_PAGE, multiplied by 300 before scroll multiplier
    handler.setScrollMultiplier(2.0);
    wheelHandler(createWheelEvent(1, 1, 2));

    // 1 page * 300 px/page = 300, then * 2.0 multiplier = 600
    expect(sent).toHaveLength(1);
    expect(sent[0]).toEqual({ t: 's', dx: 600, dy: 600 });
  });
});

/**
 * Touch-mode tests (#98 audit G6).
 *
 * Same mock-target strategy as the scroll-multiplier suite, extended with a
 * canvas (so coordinate math runs the content path) and a #zoom-layer stub
 * (the CSS-transform target for the client-side pinch-zoom). Touch handlers
 * are invoked directly with mock TouchEvent objects.
 *
 * Geometry used throughout: container + canvas rect 1280x720 at (0,0),
 * canvas intrinsic 1920x1080 — identical 16:9 aspect, so there is no
 * letterboxing and normalized coords are simply clientX/1280, clientY/720.
 */
describe('InputHandler touch modes (#98 G6)', () => {
  interface MockTouchTarget extends HTMLElement {
    __listeners: Record<string, (e: unknown) => void>;
    __zoomLayer: { style: Record<string, string> };
  }

  let sent: InputEvent[];
  let handler: InstanceType<typeof InputHandler>;
  let target: MockTouchTarget;

  function createTouchMockTarget(): MockTouchTarget {
    const listeners: Record<string, (e: unknown) => void> = {};
    const rect = { left: 0, top: 0, width: 1280, height: 720 };
    const zoomLayer = { style: {} as Record<string, string> };
    const canvas = {
      width: 1920,
      height: 1080,
      getBoundingClientRect: () => rect,
      style: {},
    };
    return {
      addEventListener: (type: string, fn: (e: unknown) => void) => {
        listeners[type] = fn;
      },
      removeEventListener: () => {},
      querySelector: (sel: string) => {
        if (sel === 'canvas') return canvas;
        if (sel === '#zoom-layer') return zoomLayer;
        return null;
      },
      getBoundingClientRect: () => rect,
      style: {},
      __listeners: listeners,
      __zoomLayer: zoomLayer,
    } as unknown as MockTouchTarget;
  }

  /** Build a mock TouchEvent from [clientX, clientY] pairs. */
  function touchEvent(points: Array<[number, number]>): TouchEvent {
    return {
      touches: points.map(([x, y]) => ({ clientX: x, clientY: y })),
      preventDefault: () => {},
    } as unknown as TouchEvent;
  }

  const start = (points: Array<[number, number]>): void =>
    target.__listeners['touchstart'](touchEvent(points));
  const move = (points: Array<[number, number]>): void =>
    target.__listeners['touchmove'](touchEvent(points));
  const end = (points: Array<[number, number]>): void =>
    target.__listeners['touchend'](touchEvent(points));

  beforeEach(() => {
    vi.stubGlobal('document', {
      addEventListener: () => {},
      removeEventListener: () => {},
      getElementById: () => null,
      pointerLockElement: null,
      fullscreenElement: null,
    });
    vi.stubGlobal(
      'ResizeObserver',
      class {
        observe() {}
        disconnect() {}
      }
    );

    sent = [];
    target = createTouchMockTarget();
    handler = new InputHandler(target, (event: InputEvent) => {
      sent.push(event);
    });
    handler.enable();
  });

  it('defaults to pointer mode and round-trips setTouchMode', () => {
    expect(handler.getTouchMode()).toBe('pointer');
    handler.setTouchMode('scrollzoom');
    expect(handler.getTouchMode()).toBe('scrollzoom');
    handler.setTouchMode('pointer');
    expect(handler.getTouchMode()).toBe('pointer');
  });

  it('pointer mode: single-finger touch emulates the mouse (regression guard)', () => {
    start([[640, 360]]);
    end([]);

    expect(sent).toEqual([
      { t: 'm', x: 0.5, y: 0.5 },
      { t: 'b', b: 0, d: true },
      { t: 'b', b: 0, d: false },
    ]);
  });

  it('pointer mode: multi-touch is ignored (regression guard)', () => {
    start([
      [600, 300],
      [700, 300],
    ]);
    move([
      [600, 250],
      [700, 250],
    ]);

    // No mouse moves, button presses, or scrolls from multi-touch.
    expect(sent).toEqual([]);

    // Pre-existing pointer-path behavior: touchend always sends a
    // button-0 release (harmless without a preceding press). Guarded
    // here so the scrollzoom refactor doesn't silently change it.
    end([]);
    expect(sent).toEqual([{ t: 'b', b: 0, d: false }]);
  });

  it('scrollzoom mode: single-finger touch sends nothing to the remote', () => {
    handler.setTouchMode('scrollzoom');
    start([[640, 360]]);
    move([[700, 400]]);
    end([]);

    expect(sent).toEqual([]);
  });

  it('scrollzoom mode: two-finger pan sends remote scroll in natural direction', () => {
    handler.setTouchMode('scrollzoom');
    start([
      [600, 500],
      [700, 500],
    ]);
    // Both fingers move up 50px: spread unchanged, midpoint travel 50px
    // (> 16px dead zone) -> classified scroll. Natural direction: fingers
    // up = positive remote dy (scroll content down).
    move([
      [600, 450],
      [700, 450],
    ]);

    expect(sent).toEqual([{ t: 's', dx: 0, dy: 50 }]);

    // Subsequent moves keep scrolling from the last midpoint.
    move([
      [600, 400],
      [700, 400],
    ]);
    expect(sent).toHaveLength(2);
    expect(sent[1]).toEqual({ t: 's', dx: 0, dy: 50 });
  });

  it('scrollzoom mode: scroll-speed multiplier applies to two-finger scroll', () => {
    handler.setTouchMode('scrollzoom');
    handler.setScrollMultiplier(2.0);
    start([
      [600, 500],
      [700, 500],
    ]);
    move([
      [600, 450],
      [700, 450],
    ]);

    expect(sent).toEqual([{ t: 's', dx: 0, dy: 100 }]);
  });

  it('scrollzoom mode: pinch outward scales the zoom layer (no remote events)', () => {
    handler.setTouchMode('scrollzoom');
    // Fingers at midpoint (650, 300), spread 100.
    start([
      [600, 300],
      [700, 300],
    ]);
    // Spread doubles to 200 (spreadDelta 100 > dead zone, midpoint still)
    // -> classified pinch -> scale 2, anchored at the midpoint.
    move([
      [550, 300],
      [750, 300],
    ]);

    expect(sent).toEqual([]);
    expect(handler.getZoomScale()).toBe(2);
    // Anchor math: content point (650, 300) stays under the midpoint:
    // tx = 650 - 650*2 = -650, ty = 300 - 300*2 = -300.
    expect(target.__zoomLayer.style.transform).toBe('translate(-650px, -300px) scale(2)');
    expect(target.__zoomLayer.style.transformOrigin).toBe('0 0');
  });

  it('scrollzoom mode: pinch zoom is clamped to the maximum scale', () => {
    handler.setTouchMode('scrollzoom');
    start([
      [640, 350],
      [660, 350],
    ]);
    // Spread explodes 20 -> 1200 (x60) — clamps at MAX_ZOOM_SCALE (5).
    move([
      [40, 350],
      [1240, 350],
    ]);

    expect(handler.getZoomScale()).toBe(5);
  });

  it('scrollzoom mode: pinching back in clamps at 1x and clears the transform', () => {
    handler.setTouchMode('scrollzoom');
    // Zoom in to 2x first.
    start([
      [600, 300],
      [700, 300],
    ]);
    move([
      [550, 300],
      [750, 300],
    ]);
    expect(handler.getZoomScale()).toBe(2);
    end([]);

    // New pinch, collapsing spread far below the starting ratio.
    start([
      [400, 300],
      [900, 300],
    ]);
    move([
      [645, 300],
      [655, 300],
    ]);

    expect(handler.getZoomScale()).toBe(1);
    expect(target.__zoomLayer.style.transform).toBe('');
    expect(sent).toEqual([]);
  });

  it('scrollzoom mode: single-finger drag pans the zoomed viewport (clamped)', () => {
    handler.setTouchMode('scrollzoom');
    // Zoom to 2x anchored at (650, 300): tx=-650, ty=-300.
    start([
      [600, 300],
      [700, 300],
    ]);
    move([
      [550, 300],
      [750, 300],
    ]);
    end([]);

    // One-finger drag right+down by (100, 50): tx -650 -> -550, ty -300 -> -250.
    start([[400, 400]]);
    move([[500, 450]]);
    expect(target.__zoomLayer.style.transform).toBe('translate(-550px, -250px) scale(2)');

    // Drag far up-left — translation clamps at the layer edges:
    // min tx = 1280*(1-2) = -1280, min ty = 720*(1-2) = -720.
    move([[-2000, -2000]]);
    expect(target.__zoomLayer.style.transform).toBe('translate(-1280px, -720px) scale(2)');

    // No remote events from any of this.
    expect(sent).toEqual([]);
  });

  it('scrollzoom mode: single-finger drag at 1x is a no-op', () => {
    handler.setTouchMode('scrollzoom');
    start([[400, 400]]);
    move([[600, 500]]);
    end([]);

    expect(sent).toEqual([]);
    expect(target.__zoomLayer.style.transform ?? '').toBe('');
  });

  it('classifies by dominance: diagonal two-finger travel with stable spread scrolls', () => {
    handler.setTouchMode('scrollzoom');
    start([
      [600, 500],
      [700, 500],
    ]);
    // Both fingers move down-right (30, 30): spread unchanged, midpoint
    // travel ~42px. Dominance picks scroll even though both axes moved.
    move([
      [630, 530],
      [730, 530],
    ]);

    expect(sent).toEqual([{ t: 's', dx: -30, dy: -30 }]);
    expect(handler.getZoomScale()).toBe(1);
  });

  it('classifies by dominance: pinch with finger tremble is not misclassified as scroll', () => {
    handler.setTouchMode('scrollzoom');
    start([
      [600, 300],
      [700, 300],
    ]);
    // Spread grows 100 -> 140 while the midpoint also drifts 10px:
    // spreadDelta (40) dominates midDelta (10) -> pinch, not scroll.
    move([
      [590, 310],
      [730, 310],
    ]);

    expect(sent).toEqual([]);
    expect(handler.getZoomScale()).toBeGreaterThan(1);
  });

  it('locks the gesture classification until the touch count changes', () => {
    handler.setTouchMode('scrollzoom');
    start([
      [600, 500],
      [700, 500],
    ]);
    // Classified as scroll...
    move([
      [600, 450],
      [700, 450],
    ]);
    expect(sent).toEqual([{ t: 's', dx: 0, dy: 50 }]);

    // ...then the fingers spread apart drastically. Still a scroll —
    // the lock holds, no zoom happens.
    move([
      [400, 400],
      [900, 400],
    ]);
    expect(handler.getZoomScale()).toBe(1);
    expect(sent).toHaveLength(2);
    expect(sent[1].t).toBe('s');
  });

  it('zoom persists across a switch back to pointer mode; touches drive the mouse again', () => {
    handler.setTouchMode('scrollzoom');
    start([
      [600, 300],
      [700, 300],
    ]);
    move([
      [550, 300],
      [750, 300],
    ]);
    end([]);
    expect(handler.getZoomScale()).toBe(2);

    handler.setTouchMode('pointer');
    // Transform untouched by the mode switch (zoom-to-click workflow).
    expect(target.__zoomLayer.style.transform).toBe('translate(-650px, -300px) scale(2)');

    start([[640, 360]]);
    end([]);
    expect(sent).toEqual([
      { t: 'm', x: 0.5, y: 0.5 },
      { t: 'b', b: 0, d: true },
      { t: 'b', b: 0, d: false },
    ]);
  });

  it('resetZoom and disable both clear the transform', () => {
    handler.setTouchMode('scrollzoom');
    start([
      [600, 300],
      [700, 300],
    ]);
    move([
      [550, 300],
      [750, 300],
    ]);
    end([]);
    expect(handler.getZoomScale()).toBe(2);

    handler.resetZoom();
    expect(handler.getZoomScale()).toBe(1);
    expect(target.__zoomLayer.style.transform).toBe('');

    // Zoom again, then disable() (session teardown) must also reset.
    start([
      [600, 300],
      [700, 300],
    ]);
    move([
      [550, 300],
      [750, 300],
    ]);
    expect(handler.getZoomScale()).toBe(2);
    handler.disable();
    expect(handler.getZoomScale()).toBe(1);
    expect(target.__zoomLayer.style.transform).toBe('');
  });

  it('2 -> 1 finger lift re-arms panning from the remaining finger', () => {
    handler.setTouchMode('scrollzoom');
    // Zoom to 2x.
    start([
      [600, 300],
      [700, 300],
    ]);
    move([
      [550, 300],
      [750, 300],
    ]);
    // Lift one finger; the remaining one is at (550, 300).
    end([[550, 300]]);
    // Pan with the remaining finger: no jump from the stale pan anchor.
    move([[560, 310]]);

    expect(target.__zoomLayer.style.transform).toBe('translate(-640px, -290px) scale(2)');
    expect(sent).toEqual([]);
  });
});
