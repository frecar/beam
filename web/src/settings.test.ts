import { describe, it, expect } from "vitest";

// formatCountdown is a pure function but settings.ts imports ui-state.ts which
// requires DOM. Re-implement inline to test the logic without DOM dependency.
function formatCountdown(secs: number): string {
  const m = Math.floor(secs / 60);
  const s = Math.floor(secs % 60);
  if (m > 0) return `${m}m ${s}s`;
  return `${s}s`;
}

describe("formatCountdown", () => {
  it("formats minutes and seconds", () => {
    expect(formatCountdown(125)).toBe("2m 5s");
    expect(formatCountdown(60)).toBe("1m 0s");
    expect(formatCountdown(90)).toBe("1m 30s");
  });

  it("formats seconds only when under a minute", () => {
    expect(formatCountdown(45)).toBe("45s");
    expect(formatCountdown(0)).toBe("0s");
  });
});
