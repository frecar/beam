/**
 * Quality mode, theme toggling, idle timeout, performance overlay,
 * network quality monitoring, and latency stats display.
 */

import type { BeamConnection } from "./connection";
import type { BeamUI } from "./ui";
import type { ConnectionState } from "./ui-state";
import {
  btnTheme, perfOverlay, remoteVideo,
  lsRtt, lsFps, lsDecode, lsLoss, lsTooltip,
  bandwidthIndicator, nqDot, idleWarning,
} from "./ui-state";

// --- Storage keys ---
export const AUDIO_MUTED_KEY = "beam_audio_muted";
export const SCROLL_SPEED_KEY = "beam_scroll_speed";
export const THEME_KEY = "beam_theme";
export const FORWARD_KEYS_KEY = "beam_forward_keys";
export const SESSION_TIMEOUT_KEY = "beam_session_timeout";
export const QUALITY_MODE_KEY = "beam_quality_mode";

// --- Idle timeout ---
export const IDLE_WARNING_BEFORE_SECS = 120; // Show warning 2 min before expiry
export const IDLE_CHECK_INTERVAL_MS = 30_000; // Check every 30s

// --- Theme (dark/light mode) ---

/** Determine if the current effective theme is light */
export function isLightMode(): boolean {
  const root = document.documentElement;
  return root.classList.contains("light-mode") ||
    (!root.classList.contains("dark-mode") &&
     window.matchMedia("(prefers-color-scheme: light)").matches);
}

/** Update the theme toggle button label to reflect the current mode */
export function updateThemeButton(): void {
  btnTheme.textContent = isLightMode() ? "Dark" : "Light";
  btnTheme.setAttribute("aria-label", isLightMode() ? "Switch to dark theme" : "Switch to light theme");
}

/** Toggle between light and dark mode, persisting the choice */
export function toggleTheme(): void {
  const root = document.documentElement;
  if (isLightMode()) {
    // Switch to dark
    root.classList.remove("light-mode");
    root.classList.add("dark-mode");
    localStorage.setItem(THEME_KEY, "dark");
  } else {
    // Switch to light
    root.classList.remove("dark-mode");
    root.classList.add("light-mode");
    localStorage.setItem(THEME_KEY, "light");
  }
  updateThemeButton();
}

/** Initialize theme from localStorage or system preference */
export function initTheme(): void {
  const saved = localStorage.getItem(THEME_KEY);
  const root = document.documentElement;
  if (saved === "light") {
    root.classList.add("light-mode");
    root.classList.remove("dark-mode");
  } else if (saved === "dark") {
    root.classList.add("dark-mode");
    root.classList.remove("light-mode");
  }
  // If no saved preference, neither class is set, so the
  // @media (prefers-color-scheme: light) rule in CSS takes effect.
  updateThemeButton();
}

// --- Network quality monitor ---

/** Compute a 0-100 network quality score from RTT and packet loss */
export function computeNetworkScore(rttMs: number | null, lossPercent: number): number {
  let rttScore = 100;
  if (rttMs !== null) {
    if (rttMs > 100) rttScore = 20;
    else if (rttMs > 50) rttScore = 50;
    else if (rttMs > 20) rttScore = 80;
  }

  let lossScore = 100;
  if (lossPercent > 1) lossScore = 20;
  else if (lossPercent > 0.1) lossScore = 60;

  return Math.round((rttScore + lossScore) / 2);
}

/** Update the network quality dot color */
export function updateNetworkQualityDot(score: number, currentConnectionState: ConnectionState): void {
  if (currentConnectionState !== "connected") {
    nqDot.classList.remove("visible");
    return;
  }
  nqDot.classList.remove("nq-good", "nq-fair", "nq-poor");
  if (score > 70) {
    nqDot.classList.add("nq-good");
  } else if (score > 40) {
    nqDot.classList.add("nq-fair");
  } else {
    nqDot.classList.add("nq-poor");
  }
  nqDot.classList.add("visible");
}

/** Update the quality select option text to reflect auto level */
export function updateQualitySelectDisplay(qualityMode: "auto" | "high" | "low", autoQualityLevel: "high" | "low"): void {
  const qualitySelect = document.getElementById("quality-select") as HTMLSelectElement | null;
  if (!qualitySelect) return;
  const autoOption = qualitySelect.querySelector('option[value="auto"]') as HTMLOptionElement | null;
  if (autoOption) {
    autoOption.textContent = qualityMode === "auto"
      ? `Auto (${autoQualityLevel === "high" ? "High" : "Low"})`
      : "Auto";
  }
}

/** Switch auto quality level and notify the agent */
export function switchAutoQuality(
  level: "high" | "low",
  autoQualityLevel: "high" | "low",
  qualityMode: "auto" | "high" | "low",
  connection: BeamConnection | null,
  ui: BeamUI | null,
): { newLevel: "high" | "low" } {
  if (autoQualityLevel === level) return { newLevel: autoQualityLevel };

  // Send quality command to agent
  connection?.sendInput({ t: "q", mode: level });

  // Update select display
  updateQualitySelectDisplay(qualityMode, level);

  // Toast notification
  if (level === "low") {
    ui?.showNotification("Quality reduced due to network conditions", "warning");
  } else {
    ui?.showNotification("Quality restored to high", "success");
  }

  return { newLevel: level };
}

// --- Format helpers ---

/** Format a byte count as a human-readable string (KB, MB, GB) */
export function formatTransferred(bytes: number): string {
  if (bytes < 1024 * 1024) {
    return `${(bytes / 1024).toFixed(0)} KB`;
  } else if (bytes < 1024 * 1024 * 1024) {
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  } else {
    return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GB`;
  }
}

/** Update the bandwidth indicator in the status bar */
export function updateBandwidthIndicator(bitrateKbps: number | null, totalBytes: number, currentConnectionState: ConnectionState): void {
  if (currentConnectionState !== "connected" || bitrateKbps === null) {
    bandwidthIndicator.classList.remove("visible");
    return;
  }

  // Format bitrate
  let bitrateStr: string;
  if (bitrateKbps >= 1000) {
    bitrateStr = `${(bitrateKbps / 1000).toFixed(1)} Mbps`;
  } else {
    bitrateStr = `${bitrateKbps} kbps`;
  }

  // Format total transferred
  const totalStr = formatTransferred(totalBytes);

  bandwidthIndicator.textContent = `\u25BC ${bitrateStr} \u00B7 ${totalStr}`;

  // Color based on bandwidth: green <5 Mbps, yellow 5-15 Mbps, red >15 Mbps
  bandwidthIndicator.classList.remove("bw-green", "bw-yellow", "bw-red");
  if (bitrateKbps < 5000) {
    bandwidthIndicator.classList.add("bw-green");
  } else if (bitrateKbps <= 15000) {
    bandwidthIndicator.classList.add("bw-yellow");
  } else {
    bandwidthIndicator.classList.add("bw-red");
  }

  bandwidthIndicator.classList.add("visible");
}

// --- Performance overlay ---

/** Update the performance overlay content with color-coded values */
export function updatePerfOverlay(perfLatency: number, perfFps: number, perfBitrate: number, perfLoss: number): void {
  const rttClass = perfLatency < 20 ? "val-good" : perfLatency < 50 ? "val-warn" : "val-bad";
  const fpsClass = perfFps >= 50 ? "val-good" : perfFps >= 25 ? "val-warn" : "val-bad";
  const lossClass = perfLoss < 0.5 ? "val-good" : perfLoss < 2 ? "val-warn" : "val-bad";

  const res = `${remoteVideo.videoWidth}x${remoteVideo.videoHeight}`;
  perfOverlay.innerHTML =
    `RTT  <span class="${rttClass}">${Math.round(perfLatency)} ms</span>\n` +
    `FPS  <span class="${fpsClass}">${Math.round(perfFps)}</span>\n` +
    `Rate <span class="val-good">${perfBitrate > 0 ? perfBitrate + " kbps" : "--"}</span>\n` +
    `Loss <span class="${lossClass}">${perfLoss}%</span>\n` +
    `Res  ${res}`;
}

// --- Latency stats display (status bar) ---


/** Update FPS and decode time in the latency stats (called from renderer callback) */
export function updateLatencyStatsFps(fps: number, decodeMs?: number): void {
  lsFps.textContent = `FPS: ${Math.round(fps)}`;
  if (decodeMs !== undefined && decodeMs > 0) {
    lsDecode.textContent = `Dec: ${decodeMs.toFixed(1)}ms`;
    lsDecode.className = "ls-stat " + (decodeMs < 5 ? "ls-good" : decodeMs <= 16 ? "ls-warn" : "ls-bad");
  }
}

// --- Idle timeout warning ---

export function showIdleWarning(idleWarningVisible: boolean): boolean {
  if (idleWarningVisible) return idleWarningVisible;
  idleWarning.classList.add("visible");
  console.warn("Idle timeout warning: session will expire soon due to inactivity");
  return true;
}

export function hideIdleWarning(idleWarningVisible: boolean): boolean {
  if (!idleWarningVisible) return idleWarningVisible;
  idleWarning.classList.remove("visible");
  return false;
}

/** Reset the latency stats display to defaults */
export function resetLatencyStats(): void {
  lsRtt.textContent = "";
  lsRtt.style.display = "none";
  lsFps.textContent = "FPS: --";
  lsDecode.textContent = "Dec: --";
  lsLoss.textContent = "";
  lsLoss.style.display = "none";
  lsTooltip.innerHTML = "";
}

/** Hide bandwidth indicator and network quality dot */
export function resetNetworkIndicators(): void {
  bandwidthIndicator.classList.remove("visible");
  nqDot.classList.remove("visible");
}
