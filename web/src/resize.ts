/** Round to even (H.264 requires even dimensions) */
export function roundToEven(n: number): number {
  return Math.floor(n / 2) * 2;
}

/** Check if a resize is significant enough to warrant a WebRTC reconnect.
 *  Returns false for the first resize (old dimensions are 0) and for
 *  changes smaller than 10% in both dimensions. */
export function isSignificantResize(
  oldW: number, oldH: number, newW: number, newH: number,
): boolean {
  if (oldW === 0 || oldH === 0) return false; // First resize, never significant
  const dw = Math.abs(newW - oldW) / oldW;
  const dh = Math.abs(newH - oldH) / oldH;
  return dw > 0.1 || dh > 0.1; // >10% change in either dimension
}
