import { describe, it, expect } from "vitest";
import { roundToEven, isSignificantResize } from "./resize";

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
