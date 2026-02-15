import { ClipboardBridge, type ClipboardHistoryEntry } from "./clipboard";
import { BeamConnection } from "./connection";
import { FileDownloader, FileUploader } from "./filetransfer";
import type { DownloadMessage } from "./filetransfer";
import { InputHandler } from "./input";
import { Renderer } from "./renderer";
import { BeamUI } from "./ui";

/** Shape of the login API response */
interface LoginResponse {
  session_id: string;
  token: string;
  release_token?: string;
  idle_timeout?: number;
}

/** Stored session with expiry timestamp */
interface StoredSession extends LoginResponse {
  saved_at: number;
}

const SESSION_KEY = "beam_session";
const SESSION_MAX_AGE_MS = 3600_000; // 1 hour — matches server reaper
const AUDIO_MUTED_KEY = "beam_audio_muted";
const SCROLL_SPEED_KEY = "beam_scroll_speed";
const THEME_KEY = "beam_theme";
const FORWARD_KEYS_KEY = "beam_forward_keys";
const SESSION_TIMEOUT_KEY = "beam_session_timeout";

// Idle timeout warning: updated from the login response idle_timeout field.
// We warn 2 minutes before expiry.
let effectiveIdleTimeoutSecs = 3600; // updated from login response
const IDLE_WARNING_BEFORE_SECS = 120; // Show warning 2 min before expiry
const IDLE_CHECK_INTERVAL_MS = 30_000; // Check every 30s

function saveSession(data: LoginResponse): void {
  const stored: StoredSession = { ...data, saved_at: Date.now() };
  localStorage.setItem(SESSION_KEY, JSON.stringify(stored));
}

function loadSession(): LoginResponse | null {
  const raw = localStorage.getItem(SESSION_KEY);
  if (!raw) return null;
  try {
    const data = JSON.parse(raw) as StoredSession;
    if (Date.now() - data.saved_at > SESSION_MAX_AGE_MS) {
      localStorage.removeItem(SESSION_KEY);
      return null;
    }
    return data;
  } catch {
    localStorage.removeItem(SESSION_KEY);
    return null;
  }
}

function clearSession(): void {
  localStorage.removeItem(SESSION_KEY);
}

// DOM elements
const loginView = document.getElementById("login-view") as HTMLDivElement;
const desktopView = document.getElementById("desktop-view") as HTMLDivElement;
const loginForm = document.getElementById("login-form") as HTMLFormElement;
const usernameInput = document.getElementById("username") as HTMLInputElement;
const passwordInput = document.getElementById("password") as HTMLInputElement;
const connectBtn = document.getElementById("connect-btn") as HTMLButtonElement;
const loginError = document.getElementById("login-error") as HTMLDivElement;
const loginCard = document.querySelector(".login-card") as HTMLDivElement;
const passwordToggle = document.getElementById("password-toggle") as HTMLButtonElement;
const sessionTimeoutSelect = document.getElementById("session-timeout") as HTMLSelectElement;
const loginFormContent = document.getElementById("login-form-content") as HTMLDivElement;
const loginLoading = document.getElementById("login-loading") as HTMLDivElement;
const loadingSpinner = document.getElementById("loading-spinner") as HTMLDivElement;
const loadingStatus = document.getElementById("loading-status") as HTMLParagraphElement;
const loadingCancel = document.getElementById("loading-cancel") as HTMLButtonElement;
const remoteVideo = document.getElementById("remote-video") as HTMLVideoElement;
const statusBar = document.getElementById("status-bar") as HTMLDivElement;
const statusDot = document.getElementById("status-dot") as HTMLDivElement;
const statusText = document.getElementById("status-text") as HTMLSpanElement;
const statusVersion = document.getElementById("status-version") as HTMLSpanElement;

const bandwidthIndicator = document.getElementById("bandwidth-indicator") as HTMLSpanElement;
const lsRtt = document.getElementById("ls-rtt") as HTMLSpanElement;
const lsFps = document.getElementById("ls-fps") as HTMLSpanElement;
const lsDecode = document.getElementById("ls-decode") as HTMLSpanElement;
const lsLoss = document.getElementById("ls-loss") as HTMLSpanElement;
const lsTooltip = document.getElementById("ls-tooltip") as HTMLDivElement;
const faviconLink = document.querySelector("link[rel='icon']") as HTMLLinkElement;

const btnMute = document.getElementById("btn-mute") as HTMLButtonElement;
const btnForwardKeys = document.getElementById("btn-forward-keys") as HTMLButtonElement;
const btnTheme = document.getElementById("btn-theme") as HTMLButtonElement;
const perfOverlay = document.getElementById("perf-overlay") as HTMLDivElement;
const helpOverlay = document.getElementById("help-overlay") as HTMLDivElement;
const sessionInfoPanel = document.getElementById("session-info-panel") as HTMLDivElement;
const sipCloseBtn = document.getElementById("sip-close") as HTMLButtonElement;
const reconnectOverlay = document.getElementById("reconnect-overlay") as HTMLDivElement;
const reconnectTitle = document.getElementById("reconnect-title") as HTMLHeadingElement;
const reconnectIcon = document.querySelector(".reconnect-icon") as HTMLDivElement;
const reconnectBtn = document.getElementById("reconnect-btn") as HTMLButtonElement;
const reconnectDisconnectBtn = document.getElementById("reconnect-disconnect-btn") as HTMLButtonElement;
const reconnectDesc = document.getElementById("reconnect-desc") as HTMLParagraphElement;
const idleWarning = document.getElementById("idle-warning") as HTMLDivElement;

let connection: BeamConnection | null = null;
let renderer: Renderer | null = null;
let inputHandler: InputHandler | null = null;
let clipboardBridge: ClipboardBridge | null = null;
let fileUploader: FileUploader | null = null;
let fileDownloader: FileDownloader | null = null;
let ui: BeamUI | null = null;
let statsInterval: ReturnType<typeof setInterval> | null = null;
let heartbeatInterval: ReturnType<typeof setInterval> | null = null;
let connectionTimeout: ReturnType<typeof setTimeout> | null = null;

// First-frame tracking: gate ResizeObserver/fullscreen resize events
// until the decoder has stabilized (prevents mid-stream resolution changes).

// Soft reconnect scheduling for resolution changes
let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
const RECONNECT_DELAY_MS = 1000; // Give agent time to process resize

// Token management for JWT refresh
let currentToken: string | null = null;
let refreshTimer: ReturnType<typeof setTimeout> | null = null;

// Release token for graceful session cleanup on tab close
let currentReleaseToken: string | null = null;
let currentSessionId: string | null = null;

// Guard against race between heartbeat 404 and user clicking reconnect
let isReturningToLogin = false;

// For calculating received video bitrate from inbound-rtp stats
let prevBytesReceived = 0;
let prevStatsTimestamp = 0;
let prevPacketsReceived = 0;
let prevPacketsLost = 0;

// Cumulative bytes received during this session (for bandwidth indicator)
let sessionBytesReceived = 0;

// Performance overlay state (updated from stats poll + renderer)
let perfFps = 0;
let perfLatency = 0;
let perfBitrate = 0;
let perfLoss = 0;

// Decode time tracking (per-frame average from inbound-rtp stats)
let prevFramesDecoded = 0;
let prevTotalDecodeTime = 0;
let currentDecodeTimeMs = 0;

// Running RTT average for tooltip
let rttSamples: number[] = [];
const RTT_SAMPLE_WINDOW = 30;

// Tooltip stats
let lastJitterMs = 0;
let lastVideoCodec = "";
let lastVideoResolution = "";
let totalPacketsReceived = 0;
let totalPacketsLost = 0;

// Track connection state so quality updates only apply when connected
let currentConnectionState: "disconnected" | "connecting" | "connected" | "error" = "disconnected";

// --- Auto quality mode ---
const QUALITY_MODE_KEY = "beam_quality_mode";
let qualityMode: "auto" | "high" | "low" = "auto";
let autoQualityLevel: "high" | "low" = "high";
let qualityScoreHistory: { score: number; time: number }[] = [];
const nqDot = document.getElementById("nq-dot") as HTMLSpanElement;

// Idle timeout warning state
let lastActivity = Date.now();
let idleCheckInterval: ReturnType<typeof setInterval> | null = null;
let idleWarningVisible = false;

// --- Theme (dark/light mode) ---

/** Determine if the current effective theme is light */
function isLightMode(): boolean {
  const root = document.documentElement;
  return root.classList.contains("light-mode") ||
    (!root.classList.contains("dark-mode") &&
     window.matchMedia("(prefers-color-scheme: light)").matches);
}

/** Update the theme toggle button label to reflect the current mode */
function updateThemeButton(): void {
  btnTheme.textContent = isLightMode() ? "Dark" : "Light";
  btnTheme.setAttribute("aria-label", isLightMode() ? "Switch to dark theme" : "Switch to light theme");
}

/** Toggle between light and dark mode, persisting the choice */
function toggleTheme(): void {
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
function initTheme(): void {
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

// Initialize theme immediately (before any async work)
initTheme();

// Listen for system theme changes (only matters when no explicit preference is saved)
window.matchMedia("(prefers-color-scheme: light)").addEventListener("change", () => {
  const saved = localStorage.getItem(THEME_KEY);
  if (!saved) {
    updateThemeButton();
  }
});

// Clipboard history panel state
let clipboardHistoryVisible = false;
const clipboardHistoryPanel = document.getElementById("clipboard-history-panel") as HTMLDivElement;
const chpList = document.getElementById("chp-list") as HTMLDivElement;
const chpClearBtn = document.getElementById("chp-clear") as HTMLButtonElement;
const chpCloseBtn = document.getElementById("chp-close") as HTMLButtonElement;

// Admin sessions panel state
let adminPanelVisible = false;
let adminRefreshInterval: ReturnType<typeof setInterval> | null = null;
const adminPanelOverlay = document.getElementById("admin-panel-overlay") as HTMLDivElement;
const adminSessionsTbody = document.getElementById("admin-sessions-tbody") as HTMLTableSectionElement;
const adminSessionCount = document.getElementById("admin-session-count") as HTMLSpanElement;
const adminPanelClose = document.getElementById("admin-panel-close") as HTMLButtonElement;

// Session info panel state
let sessionInfoVisible = false;
let connectedSinceTime: number | null = null;
let sessionDurationInterval: ReturnType<typeof setInterval> | null = null;
let sessionUsername: string | null = null;

// Audio stats tracking for session info panel
let prevAudioBytesReceived = 0;
let prevAudioStatsTimestamp = 0;

/** Generate an SVG data URL for a colored circle favicon */
function faviconDataUrl(color: string): string {
  return `data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'%3E%3Ccircle cx='8' cy='8' r='7' fill='${encodeURIComponent(color)}'/%3E%3C/svg%3E`;
}

function setFavicon(color: string): void {
  if (faviconLink) {
    faviconLink.href = faviconDataUrl(color);
  }
}

function setStatus(state: "disconnected" | "connecting" | "connected" | "error", message: string): void {
  currentConnectionState = state;
  statusText.textContent = message;
  statusDot.className = "status-dot";
  statusDot.style.backgroundColor = "";

  switch (state) {
    case "connected":
      statusDot.classList.add("connected");
      document.title = "Beam - Connected";
      setFavicon("#4ade80"); // green
      break;
    case "connecting":
      statusDot.classList.add("connecting");
      document.title = "Beam - Disconnected";
      setFavicon("#facc15"); // yellow
      break;
    case "error":
      statusDot.classList.add("error");
      document.title = "Beam - Disconnected";
      setFavicon("#ff6b6b"); // red
      break;
    case "disconnected":
    default:
      document.title = "Beam - Login";
      setFavicon("#888"); // gray
      break;
  }
}

/** Update status bar dot color and text based on current RTT latency */
function updateConnectionQuality(rttMs: number): void {
  if (currentConnectionState !== "connected") return;

  if (rttMs > 80) {
    statusDot.style.backgroundColor = "#ff6b6b";
    statusText.textContent = "Connected (slow)";
  } else if (rttMs >= 30) {
    statusDot.style.backgroundColor = "#facc15";
    statusText.textContent = "Connected";
  } else {
    statusDot.style.backgroundColor = "#4ade80";
    statusText.textContent = "Connected";
  }
}

// --- Network quality monitor ---

/** Compute a 0-100 network quality score from RTT and packet loss */
function computeNetworkScore(rttMs: number | null, lossPercent: number): number {
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
function updateNetworkQualityDot(score: number): void {
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
function updateQualitySelectDisplay(): void {
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
function switchAutoQuality(level: "high" | "low"): void {
  if (autoQualityLevel === level) return;
  autoQualityLevel = level;

  // Send quality command to agent
  connection?.sendInput({ t: "q", mode: level });

  // Update select display
  updateQualitySelectDisplay();

  // Toast notification
  if (level === "low") {
    ui?.showNotification("Quality reduced due to network conditions", "warning");
  } else {
    ui?.showNotification("Quality restored to high", "success");
  }
}

/** Feed stats to the network quality monitor (called from pollWebRTCStats) */
function updateNetworkQualityMonitor(rttMs: number | null, lossPercent: number): void {
  const score = computeNetworkScore(rttMs, lossPercent);
  const now = Date.now();

  // Always update the dot (visible regardless of auto mode)
  updateNetworkQualityDot(score);

  if (qualityMode !== "auto") return;

  qualityScoreHistory.push({ score, time: now });
  // Keep last 15 seconds of history
  qualityScoreHistory = qualityScoreHistory.filter(s => now - s.time < 15_000);

  if (autoQualityLevel === "high") {
    // Drop to low: score < 40 sustained for 5 seconds
    const fiveSecsAgo = now - 5_000;
    const recent = qualityScoreHistory.filter(s => s.time >= fiveSecsAgo);
    if (recent.length >= 3 && recent.every(s => s.score < 40)) {
      switchAutoQuality("low");
    }
  } else {
    // Restore to high: score > 70 sustained for 10 seconds
    const tenSecsAgo = now - 10_000;
    const recent = qualityScoreHistory.filter(s => s.time >= tenSecsAgo);
    if (recent.length >= 5 && recent.every(s => s.score > 70)) {
      switchAutoQuality("high");
    }
  }
}

/** Format a byte count as a human-readable string (KB, MB, GB) */
function formatTransferred(bytes: number): string {
  if (bytes < 1024 * 1024) {
    return `${(bytes / 1024).toFixed(0)} KB`;
  } else if (bytes < 1024 * 1024 * 1024) {
    return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  } else {
    return `${(bytes / (1024 * 1024 * 1024)).toFixed(1)} GB`;
  }
}

/** Update the bandwidth indicator in the status bar */
function updateBandwidthIndicator(bitrateKbps: number | null, totalBytes: number): void {
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

function setToken(token: string): void {
  currentToken = token;
  const data = loadSession();
  if (data) {
    data.token = token;
    saveSession(data);
  }
}

/** Send release beacon to start server-side grace period cleanup.
 *  Uses navigator.sendBeacon() which reliably fires during tab close. */
function sendReleaseBeacon(): void {
  if (currentSessionId && currentReleaseToken) {
    navigator.sendBeacon(
      `/api/sessions/${currentSessionId}/release`,
      currentReleaseToken,
    );
  }
}

/** Parse JWT exp claim without verification */
function parseJwtExp(token: string): number | null {
  try {
    const payload = token.split(".")[1];
    const decoded = JSON.parse(atob(payload)) as { exp?: number };
    return decoded.exp ?? null;
  } catch {
    return null;
  }
}

/** Schedule proactive token refresh 5 minutes before expiry */
function scheduleTokenRefresh(): void {
  if (refreshTimer) {
    clearTimeout(refreshTimer);
    refreshTimer = null;
  }
  if (!currentToken) return;

  const exp = parseJwtExp(currentToken);
  if (!exp) return;

  const nowSec = Math.floor(Date.now() / 1000);
  const refreshInMs = (exp - nowSec - 300) * 1000; // 5 min before expiry

  if (refreshInMs <= 0) {
    refreshToken();
    return;
  }

  refreshTimer = setTimeout(() => {
    refreshTimer = null;
    refreshToken();
  }, refreshInMs);
}

/** Attempt to refresh the JWT token */
async function refreshToken(): Promise<boolean> {
  if (!currentToken) return false;
  try {
    const resp = await fetch("/api/auth/refresh", {
      method: "POST",
      headers: { Authorization: `Bearer ${currentToken}` },
    });
    if (!resp.ok) return false;
    const data = (await resp.json()) as { token: string };
    setToken(data.token);
    connection?.updateToken(data.token);
    scheduleTokenRefresh();
    console.log("Token refreshed");
    return true;
  } catch {
    console.warn("Token refresh failed");
    return false;
  }
}

function showLoginError(message: string): void {
  loginError.textContent = message;
  loginError.style.display = "block";
}

function hideLoginError(): void {
  loginError.style.display = "none";
}

function showLoading(message: string): void {
  hideLoginError();
  loginFormContent.style.display = "none";
  loginLoading.style.display = "flex";
  loadingSpinner.className = "loading-spinner";
  loadingStatus.className = "loading-status";
  loadingStatus.textContent = message;
  loadingCancel.textContent = "Cancel";
}

function updateLoadingStatus(message: string): void {
  loadingStatus.style.opacity = "0";
  setTimeout(() => {
    loadingStatus.textContent = message;
    loadingStatus.style.opacity = "1";
  }, 150);
}

function shakeLoginCard(): void {
  loginCard.classList.remove("shake");
  // Force reflow so re-adding the class restarts the animation
  void loginCard.offsetWidth;
  loginCard.classList.add("shake");
  loginCard.addEventListener("animationend", () => loginCard.classList.remove("shake"), { once: true });
}

function showLoadingError(message: string): void {
  loadingSpinner.classList.add("error");
  loadingStatus.textContent = message;
  loadingStatus.classList.add("error");
  shakeLoginCard();
  loadingCancel.textContent = "Back to login";
}

function hideLoading(): void {
  loginLoading.style.display = "none";
  loginFormContent.style.display = "block";
  loadingSpinner.className = "loading-spinner";
  loadingStatus.className = "loading-status";
  loadingCancel.textContent = "Cancel";
  connectBtn.disabled = false;
  connectBtn.textContent = "Connect";
}

function showDesktop(): void {
  loginView.style.display = "none";
  desktopView.style.display = "block";
  statusBar.classList.add("visible");
  if (isTouchDevice) {
    mobileFab.classList.add("visible");
  }
  if (connectionTimeout) {
    clearTimeout(connectionTimeout);
    connectionTimeout = null;
  }
  // Fetch and display server version (best-effort, non-blocking)
  fetch("/api/health")
    .then((r) => r.json())
    .then((data: { version?: string }) => {
      if (data.version) statusVersion.textContent = `v${data.version}`;
    })
    .catch(() => {});
}

function showLogin(): void {
  loginView.style.display = "flex";
  desktopView.style.display = "none";
  statusBar.classList.remove("visible");
  mobileFab.classList.remove("visible");
  closeFab();
  hideLoading();
}

/** Extract round-trip latency and quality metrics from WebRTC stats */
async function pollWebRTCStats(): Promise<void> {
  if (!connection) return;

  const stats = await connection.getStats();
  if (!stats) return;

  let rttMs: number | null = null;
  let bitrateKbps: number | null = null;
  let packetsLost = 0;
  let packetsReceived = 0;
  let currentBytesReceived = 0;
  let currentTimestamp = 0;
  let framesDecoded = 0;
  let totalDecodeTime = 0;
  let jitterSec = 0;
  let videoCodecId = "";

  stats.forEach((report) => {
    if (report.type === "candidate-pair" && report.state === "succeeded") {
      const rtt = report.currentRoundTripTime;
      if (typeof rtt === "number") {
        rttMs = rtt * 1000;
      }
    }

    // Track inbound video stats (we're receiving, not sending video)
    if (report.type === "inbound-rtp" && report.kind === "video") {
      packetsReceived = report.packetsReceived || 0;
      packetsLost = report.packetsLost || 0;
      currentBytesReceived = report.bytesReceived || 0;
      currentTimestamp = report.timestamp;
      framesDecoded = report.framesDecoded || 0;
      totalDecodeTime = report.totalDecodeTime || 0;
      if (typeof report.jitter === "number") {
        jitterSec = report.jitter;
      }
      videoCodecId = report.codecId || "";
    }

  });

  // Resolve video codec name from codec stats
  if (videoCodecId) {
    stats.forEach((report) => {
      if (report.type === "codec" && report.id === videoCodecId) {
        lastVideoCodec = (report.mimeType || "").replace("video/", "");
      }
    });
  }

  // Calculate actual received video bitrate from inbound-rtp delta
  if (currentBytesReceived > 0 && prevBytesReceived > 0 && currentTimestamp > prevStatsTimestamp) {
    const deltaBytes = currentBytesReceived - prevBytesReceived;
    const deltaSec = (currentTimestamp - prevStatsTimestamp) / 1000;
    if (deltaSec > 0) {
      bitrateKbps = Math.round((deltaBytes * 8) / deltaSec / 1000);
    }
    sessionBytesReceived += deltaBytes;
  }
  prevBytesReceived = currentBytesReceived;
  prevStatsTimestamp = currentTimestamp;

  // Calculate per-frame decode time
  if (framesDecoded > prevFramesDecoded && totalDecodeTime > prevTotalDecodeTime) {
    const deltaFrames = framesDecoded - prevFramesDecoded;
    const deltaTime = totalDecodeTime - prevTotalDecodeTime;
    currentDecodeTimeMs = (deltaTime / deltaFrames) * 1000;
  }
  prevFramesDecoded = framesDecoded;
  prevTotalDecodeTime = totalDecodeTime;

  // RTT running average for tooltip
  if (rttMs !== null) {
    rttSamples.push(rttMs);
    if (rttSamples.length > RTT_SAMPLE_WINDOW) rttSamples.shift();
  }

  // Store stats for tooltip
  lastJitterMs = jitterSec * 1000;
  totalPacketsReceived = packetsReceived;
  totalPacketsLost = packetsLost;
  if (remoteVideo.videoWidth > 0) {
    lastVideoResolution = `${remoteVideo.videoWidth}x${remoteVideo.videoHeight}`;
  }

  // Calculate interval loss percentage for better reactivity in auto-quality
  let intervalLossPercent = 0;
  const deltaReceived = packetsReceived - prevPacketsReceived;
  const deltaLost = Math.max(0, packetsLost - prevPacketsLost);
  if (deltaReceived + deltaLost > 0) {
    intervalLossPercent = (deltaLost / (deltaReceived + deltaLost)) * 100;
  }
  prevPacketsReceived = packetsReceived;
  prevPacketsLost = packetsLost;

  // Calculate cumulative loss percentage for display
  const cumulativeLossPercent =
    packetsReceived > 0 ? (packetsLost / packetsReceived) * 100 : 0;

  // Update latency stats display in status bar
  updateLatencyStats(rttMs, currentDecodeTimeMs, cumulativeLossPercent);

  // Update performance overlay state
  if (rttMs !== null) perfLatency = rttMs;
  if (bitrateKbps !== null) perfBitrate = bitrateKbps;
  perfLoss = Math.round(cumulativeLossPercent * 10) / 10;
  updatePerfOverlay();

  // Update status bar connection quality indicator based on latency
  if (rttMs !== null) updateConnectionQuality(rttMs);

  // Update bandwidth indicator in status bar
  updateBandwidthIndicator(bitrateKbps, sessionBytesReceived);

  // Feed the network quality monitor with interval loss
  updateNetworkQualityMonitor(rttMs, intervalLossPercent);

  // Warn if video element has no frames decoded yet (debugging aid)
  if (remoteVideo.srcObject && remoteVideo.videoWidth === 0 && remoteVideo.videoHeight === 0) {
    console.warn("Video element has srcObject but 0x0 dimensions - no frames decoded yet");
  }

  // Update session info panel if visible (reuses the same 2s polling interval)
  updateSessionInfoStats();
}

/** Update the performance overlay content with color-coded values */
function updatePerfOverlay(): void {
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

function tooltipRow(label: string, value: string): string {
  return `<div class="ls-tooltip-row"><span class="ls-tooltip-label">${label}</span><span>${value}</span></div>`;
}

/** Update the compact latency stats display in the status bar */
function updateLatencyStats(rttMs: number | null, decodeMs: number, lossPercent: number): void {
  if (rttMs !== null) {
    const rttRounded = Math.round(rttMs);
    lsRtt.textContent = `RTT: ${rttRounded}ms`;
    lsRtt.className = "ls-stat " + (rttMs < 20 ? "ls-good" : rttMs <= 50 ? "ls-warn" : "ls-bad");
  }

  if (decodeMs > 0) {
    lsDecode.textContent = `Dec: ${decodeMs.toFixed(1)}ms`;
  }

  lsLoss.textContent = `Loss: ${lossPercent.toFixed(1)}%`;

  // Update tooltip with detailed stats
  const avgRtt = rttSamples.length > 0
    ? Math.round(rttSamples.reduce((a, b) => a + b, 0) / rttSamples.length)
    : null;

  lsTooltip.innerHTML = [
    tooltipRow("RTT", rttMs !== null ? `${Math.round(rttMs)} ms${avgRtt !== null ? ` (avg: ${avgRtt} ms)` : ""}` : "--"),
    tooltipRow("Jitter", `${lastJitterMs.toFixed(1)} ms`),
    tooltipRow("FPS", `${Math.round(perfFps)}`),
    tooltipRow("Decode", decodeMs > 0 ? `${decodeMs.toFixed(1)} ms` : "--"),
    tooltipRow("Received", `${totalPacketsReceived.toLocaleString()} pkts`),
    tooltipRow("Lost", `${totalPacketsLost.toLocaleString()} (${lossPercent.toFixed(2)}%)`),
    tooltipRow("Resolution", lastVideoResolution || "--"),
    tooltipRow("Codec", lastVideoCodec || "--"),
  ].join("");
}

/** Update just the FPS in the latency stats (called from renderer callback) */
function updateLatencyStatsFps(fps: number): void {
  lsFps.textContent = `FPS: ${Math.round(fps)}`;
}

// --- Session info panel ---

function toggleSessionInfoPanel(): void {
  sessionInfoVisible = !sessionInfoVisible;
  if (sessionInfoVisible) {
    sessionInfoPanel.classList.add("visible");
    updateSessionInfoPanel();
    startSessionDurationTimer();
  } else {
    sessionInfoPanel.classList.remove("visible");
    stopSessionDurationTimer();
  }
}

function hideSessionInfoPanel(): void {
  sessionInfoVisible = false;
  sessionInfoPanel.classList.remove("visible");
  stopSessionDurationTimer();
}

// --- Clipboard history panel ---

function toggleClipboardHistoryPanel(): void {
  clipboardHistoryVisible = !clipboardHistoryVisible;
  if (clipboardHistoryVisible) {
    clipboardHistoryPanel.classList.add("visible");
    renderClipboardHistory();
  } else {
    clipboardHistoryPanel.classList.remove("visible");
  }
}

function hideClipboardHistoryPanel(): void {
  clipboardHistoryVisible = false;
  clipboardHistoryPanel.classList.remove("visible");
}

/** Format a timestamp as HH:MM:SS */
function formatTime(ts: number): string {
  return new Date(ts).toLocaleTimeString();
}

/** Render the clipboard history list from the ClipboardBridge */
function renderClipboardHistory(): void {
  if (!clipboardHistoryVisible) return;

  const history: ClipboardHistoryEntry[] = clipboardBridge?.getHistory() ?? [];
  if (history.length === 0) {
    chpList.innerHTML = '<div class="chp-empty">No clipboard activity yet</div>';
    return;
  }

  // Render newest-first
  const html = history.slice().reverse().map((entry, idx) => {
    const arrow = entry.direction === "sent" ? "\u2192" : "\u2190";
    const dirClass = entry.direction;
    const preview = ClipboardBridge.truncatePreview(entry.text);
    // Escape HTML to prevent XSS from clipboard content
    const escaped = preview
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;");
    return `<div class="chp-entry">
      <div class="chp-entry-header">
        <div class="chp-entry-meta">
          <span class="chp-direction ${dirClass}">${arrow}</span>
          <span>${formatTime(entry.timestamp)}</span>
          <span>${entry.direction === "sent" ? "Sent" : "Received"}</span>
        </div>
        <button class="chp-copy" data-chp-idx="${idx}" aria-label="Copy to clipboard">Copy</button>
      </div>
      <div class="chp-text">${escaped}</div>
    </div>`;
  }).join("");

  chpList.innerHTML = html;

  // Wire copy buttons
  chpList.querySelectorAll(".chp-copy").forEach((btn) => {
    btn.addEventListener("click", () => {
      const idx = parseInt((btn as HTMLElement).dataset.chpIdx || "0", 10);
      const reversedHistory = history.slice().reverse();
      const entry = reversedHistory[idx];
      if (entry) {
        navigator.clipboard.writeText(entry.text).then(
          () => ui?.showNotification("Copied to clipboard", "success", 1500),
          () => ui?.showNotification("Failed to copy", "error"),
        );
      }
    });
  });
}

// --- Admin sessions panel ---

/** Shape of the admin session list API response */
interface AdminSession {
  id: string;
  username: string;
  display: number;
  created_at: number;
  last_activity: number;
}

/** Format a Unix epoch timestamp as a relative time string ("2m ago", "1h ago") */
function formatRelativeTime(epochSecs: number): string {
  const deltaSecs = Math.floor(Date.now() / 1000) - epochSecs;
  if (deltaSecs < 60) return `${deltaSecs}s ago`;
  if (deltaSecs < 3600) return `${Math.floor(deltaSecs / 60)}m ago`;
  if (deltaSecs < 86400) return `${Math.floor(deltaSecs / 3600)}h ago`;
  return `${Math.floor(deltaSecs / 86400)}d ago`;
}

function toggleAdminPanel(): void {
  adminPanelVisible = !adminPanelVisible;
  if (adminPanelVisible) {
    adminPanelOverlay.classList.add("visible");
    fetchAdminSessions();
    adminRefreshInterval = setInterval(fetchAdminSessions, 10_000);
  } else {
    hideAdminPanel();
  }
}

function hideAdminPanel(): void {
  adminPanelVisible = false;
  adminPanelOverlay.classList.remove("visible");
  if (adminRefreshInterval) {
    clearInterval(adminRefreshInterval);
    adminRefreshInterval = null;
  }
}

async function fetchAdminSessions(): Promise<void> {
  if (!currentToken) {
    adminSessionsTbody.innerHTML = '<tr><td colspan="6" class="admin-empty">Not authenticated</td></tr>';
    return;
  }

  try {
    const resp = await fetch("/api/admin/sessions", {
      headers: { Authorization: `Bearer ${currentToken}` },
    });
    if (!resp.ok) {
      if (resp.status === 401) {
        adminSessionsTbody.innerHTML = '<tr><td colspan="6" class="admin-empty">Session expired</td></tr>';
        return;
      }
      throw new Error(`HTTP ${resp.status}`);
    }
    const sessions = (await resp.json()) as AdminSession[];
    renderAdminSessions(sessions);
  } catch {
    adminSessionsTbody.innerHTML = '<tr><td colspan="6" class="admin-empty">Failed to load sessions</td></tr>';
  }
}

function renderAdminSessions(sessions: AdminSession[]): void {
  adminSessionCount.textContent = String(sessions.length);

  if (sessions.length === 0) {
    adminSessionsTbody.innerHTML = '<tr><td colspan="6" class="admin-empty">No active sessions</td></tr>';
    return;
  }

  adminSessionsTbody.innerHTML = sessions.map((s) => {
    const shortId = s.id.substring(0, 8);
    const created = formatRelativeTime(s.created_at);
    const idle = formatRelativeTime(s.last_activity);
    const isSelf = s.id === currentSessionId;
    const escapedId = s.id.replace(/"/g, "&quot;");
    return `<tr>
      <td title="${escapedId}">${shortId}${isSelf ? " *" : ""}</td>
      <td>${s.username}</td>
      <td>:${s.display}</td>
      <td>${created}</td>
      <td>${idle}</td>
      <td><button class="admin-terminate-btn" data-session-id="${escapedId}"${isSelf ? ' title="This is your session"' : ""}>Terminate</button></td>
    </tr>`;
  }).join("");

  // Wire terminate buttons
  adminSessionsTbody.querySelectorAll(".admin-terminate-btn").forEach((btn) => {
    btn.addEventListener("click", () => {
      const sessionId = (btn as HTMLElement).dataset.sessionId;
      if (sessionId) {
        terminateAdminSession(sessionId, btn as HTMLButtonElement);
      }
    });
  });
}

async function terminateAdminSession(sessionId: string, btn: HTMLButtonElement): Promise<void> {
  const isSelf = sessionId === currentSessionId;
  const msg = isSelf
    ? "This is YOUR active session. Terminate it?"
    : "Terminate this session? The user will be disconnected.";
  if (!confirm(msg)) return;

  btn.disabled = true;
  btn.textContent = "...";

  try {
    const resp = await fetch(`/api/admin/sessions/${sessionId}`, {
      method: "DELETE",
      headers: { Authorization: `Bearer ${currentToken!}` },
    });
    if (resp.ok) {
      ui?.showNotification("Session terminated", "success");
      if (isSelf) {
        hideAdminPanel();
        handleDisconnect();
        return;
      }
      fetchAdminSessions();
    } else if (resp.status === 404) {
      ui?.showNotification("Session already ended", "info");
      fetchAdminSessions();
    } else {
      throw new Error(`HTTP ${resp.status}`);
    }
  } catch {
    ui?.showNotification("Failed to terminate session", "error");
    btn.disabled = false;
    btn.textContent = "Terminate";
  }
}

/** Format a duration in ms as "Xh Ym Zs" */
function formatDuration(ms: number): string {
  const totalSec = Math.floor(ms / 1000);
  const hours = Math.floor(totalSec / 3600);
  const minutes = Math.floor((totalSec % 3600) / 60);
  const seconds = totalSec % 60;
  if (hours > 0) {
    return `${hours}h ${minutes}m ${seconds}s`;
  } else if (minutes > 0) {
    return `${minutes}m ${seconds}s`;
  }
  return `${seconds}s`;
}

function startSessionDurationTimer(): void {
  stopSessionDurationTimer();
  sessionDurationInterval = setInterval(() => {
    updateSessionDuration();
  }, 1000);
}

function stopSessionDurationTimer(): void {
  if (sessionDurationInterval) {
    clearInterval(sessionDurationInterval);
    sessionDurationInterval = null;
  }
}

function updateSessionDuration(): void {
  const el = document.getElementById("sip-duration");
  if (el && connectedSinceTime) {
    el.textContent = formatDuration(Date.now() - connectedSinceTime);
  }
}

/** Populate the session info panel with current metadata */
function updateSessionInfoPanel(): void {
  const sipSessionId = document.getElementById("sip-session-id");
  const sipUsername = document.getElementById("sip-username");
  const sipConnectedSince = document.getElementById("sip-connected-since");

  if (sipSessionId && currentSessionId) {
    // Show shortened session ID (first 8 chars)
    sipSessionId.textContent = currentSessionId.substring(0, 8);
    sipSessionId.title = currentSessionId;
  }
  if (sipUsername && sessionUsername) {
    sipUsername.textContent = sessionUsername;
  }
  if (sipConnectedSince && connectedSinceTime) {
    sipConnectedSince.textContent = new Date(connectedSinceTime).toLocaleTimeString();
  }

  updateSessionDuration();
}

/** Update the session info panel with WebRTC stats (called from pollWebRTCStats) */
async function updateSessionInfoStats(): Promise<void> {
  if (!sessionInfoVisible || !connection) return;

  const stats = await connection.getStats();
  if (!stats) return;

  let iceState = "--";
  let dtlsState = "--";
  let localCandidateId = "";
  let remoteCandidateId = "";
  let localCandidateType = "--";
  let remoteCandidateType = "--";
  let transportProtocol = "--";

  // Video stats
  let videoCodec = "--";
  let videoCodecId = "";
  let videoResolution = "--";
  let videoFramerate = "--";
  let videoBitrate = "--";
  let videoPacketsLost = "--";
  let videoJitter = "--";

  // Audio stats
  let audioCodec = "--";
  let audioCodecId = "";
  let audioBitrate = "--";

  let currentAudioBytesReceived = 0;
  let currentAudioTimestamp = 0;

  stats.forEach((report) => {
    // ICE candidate pair (active)
    if (report.type === "candidate-pair" && report.state === "succeeded") {
      localCandidateId = report.localCandidateId || "";
      remoteCandidateId = report.remoteCandidateId || "";
    }

    // Transport (DTLS state, ICE state)
    if (report.type === "transport") {
      iceState = report.iceState || report.iceLocalCandidateId ? "connected" : "--";
      dtlsState = report.dtlsState || "--";
      if (report.selectedCandidatePairId) {
        // Transport-level ICE state
        iceState = report.iceState || iceState;
      }
    }

    // Local candidate
    if (report.type === "local-candidate") {
      if (report.id === localCandidateId) {
        localCandidateType = report.candidateType || "--";
        transportProtocol = (report.protocol || "--").toUpperCase();
      }
    }

    // Remote candidate
    if (report.type === "remote-candidate") {
      if (report.id === remoteCandidateId) {
        remoteCandidateType = report.candidateType || "--";
      }
    }

    // Inbound video RTP
    if (report.type === "inbound-rtp" && report.kind === "video") {
      videoCodecId = report.codecId || "";
      const fw = report.frameWidth;
      const fh = report.frameHeight;
      if (fw && fh) {
        videoResolution = `${fw}x${fh}`;
      }
      const fps = report.framesPerSecond;
      if (typeof fps === "number") {
        videoFramerate = `${Math.round(fps)} fps`;
      }
      const lost = report.packetsLost || 0;
      const received = report.packetsReceived || 0;
      videoPacketsLost = `${lost} / ${lost + received}`;
      const jitter = report.jitter;
      if (typeof jitter === "number") {
        videoJitter = `${(jitter * 1000).toFixed(1)} ms`;
      }
    }

    // Inbound audio RTP
    if (report.type === "inbound-rtp" && report.kind === "audio") {
      audioCodecId = report.codecId || "";
      currentAudioBytesReceived = report.bytesReceived || 0;
      currentAudioTimestamp = report.timestamp;
    }
  });

  // Resolve codec names from codec stats
  stats.forEach((report) => {
    if (report.type === "codec") {
      if (report.id === videoCodecId) {
        const mime = report.mimeType || "";
        videoCodec = mime.replace("video/", "");
      }
      if (report.id === audioCodecId) {
        const mime = report.mimeType || "";
        audioCodec = mime.replace("audio/", "");
      }
    }
  });

  // Calculate video bitrate from existing perf state
  if (perfBitrate > 0) {
    videoBitrate = perfBitrate >= 1000
      ? `${(perfBitrate / 1000).toFixed(1)} Mbps`
      : `${perfBitrate} kbps`;
  }

  // Calculate audio bitrate
  if (currentAudioBytesReceived > 0 && prevAudioBytesReceived > 0 && currentAudioTimestamp > prevAudioStatsTimestamp) {
    const deltaBytes = currentAudioBytesReceived - prevAudioBytesReceived;
    const deltaSec = (currentAudioTimestamp - prevAudioStatsTimestamp) / 1000;
    if (deltaSec > 0) {
      const audioKbps = Math.round((deltaBytes * 8) / deltaSec / 1000);
      audioBitrate = `${audioKbps} kbps`;
    }
  }
  prevAudioBytesReceived = currentAudioBytesReceived;
  prevAudioStatsTimestamp = currentAudioTimestamp;

  // Audio muted state (renderer controls the video element's muted property)
  const sipAudioMuted = document.getElementById("sip-audio-muted");
  if (sipAudioMuted) {
    sipAudioMuted.textContent = remoteVideo.muted ? "Yes" : "No";
  }

  // Update DOM elements
  const setText = (id: string, text: string) => {
    const el = document.getElementById(id);
    if (el) el.textContent = text;
  };

  setText("sip-ice-state", iceState);
  setText("sip-local-candidate", localCandidateType);
  setText("sip-remote-candidate", remoteCandidateType);
  setText("sip-transport", transportProtocol);
  setText("sip-dtls-state", dtlsState);

  setText("sip-resolution", videoResolution);
  setText("sip-framerate", videoFramerate);
  setText("sip-video-codec", videoCodec);
  setText("sip-video-bitrate", videoBitrate);
  setText("sip-packets-lost", videoPacketsLost);
  setText("sip-jitter", videoJitter);

  setText("sip-audio-codec", audioCodec);
  setText("sip-audio-bitrate", audioBitrate);

  // Apply color classes for ICE state
  const iceEl = document.getElementById("sip-ice-state");
  if (iceEl) {
    iceEl.classList.remove("sip-good", "sip-warn", "sip-bad", "sip-dim");
    if (iceState === "connected" || iceState === "completed") {
      iceEl.classList.add("sip-good");
    } else if (iceState === "checking" || iceState === "new") {
      iceEl.classList.add("sip-warn");
    } else if (iceState === "failed" || iceState === "disconnected") {
      iceEl.classList.add("sip-bad");
    }
  }

  // Apply color for DTLS state
  const dtlsEl = document.getElementById("sip-dtls-state");
  if (dtlsEl) {
    dtlsEl.classList.remove("sip-good", "sip-warn", "sip-bad", "sip-dim");
    if (dtlsState === "connected") {
      dtlsEl.classList.add("sip-good");
    } else if (dtlsState === "connecting" || dtlsState === "new") {
      dtlsEl.classList.add("sip-warn");
    } else if (dtlsState === "failed" || dtlsState === "closed") {
      dtlsEl.classList.add("sip-bad");
    }
  }
}

/** Collect all current stats into a formatted text block and copy to clipboard */
function copyStatsToClipboard(): void {
  const getText = (id: string): string => {
    const el = document.getElementById(id);
    return el ? el.textContent || "--" : "--";
  };

  // Session info
  const sessionId = currentSessionId || "--";
  const username = sessionUsername || "--";
  const connectedSince = connectedSinceTime
    ? new Date(connectedSinceTime).toISOString().replace("T", " ").replace(/\.\d+Z$/, "")
    : "--";
  const duration = connectedSinceTime ? formatDuration(Date.now() - connectedSinceTime) : "--";

  // Connection
  const iceState = getText("sip-ice-state");
  const transport = getText("sip-transport");
  const localCandidate = getText("sip-local-candidate");
  const remoteCandidate = getText("sip-remote-candidate");
  const dtlsState = getText("sip-dtls-state");

  // Video
  const resolution = getText("sip-resolution");
  const framerate = getText("sip-framerate");
  const videoCodec = getText("sip-video-codec");
  const videoBitrate = getText("sip-video-bitrate");
  const packetsLost = getText("sip-packets-lost");
  const jitter = getText("sip-jitter");

  // Audio
  const audioCodec = getText("sip-audio-codec");
  const audioBitrate = getText("sip-audio-bitrate");
  const audioMuted = getText("sip-audio-muted");

  // Client info
  const userAgent = navigator.userAgent;
  const screenSize = `${window.screen.width}x${window.screen.height}`;

  const text = [
    "Beam Remote Desktop - Session Stats",
    "====================================",
    `Session ID: ${sessionId}`,
    `Username: ${username}`,
    `Connected: ${connectedSince}`,
    `Duration: ${duration}`,
    "",
    "Connection:",
    `  ICE State: ${iceState}`,
    `  Transport: ${transport} (${localCandidate} \u2192 ${remoteCandidate})`,
    `  DTLS: ${dtlsState}`,
    "",
    "Video:",
    `  Resolution: ${resolution}`,
    `  Framerate: ${framerate}`,
    `  Codec: ${videoCodec}`,
    `  Bitrate: ${videoBitrate}`,
    `  Packets lost: ${packetsLost}`,
    `  Jitter: ${jitter}`,
    "",
    "Audio:",
    `  Codec: ${audioCodec}`,
    `  Bitrate: ${audioBitrate}`,
    `  Muted: ${audioMuted}`,
    "",
    "Client:",
    `  User Agent: ${userAgent}`,
    `  Screen: ${screenSize}`,
  ].join("\n");

  navigator.clipboard.writeText(text).then(
    () => {
      ui?.showNotification("Stats copied to clipboard", "success");
    },
    () => {
      ui?.showNotification("Failed to copy stats to clipboard", "error");
    },
  );
}

function startHeartbeat(sessionId: string): void {
  stopHeartbeat();
  heartbeatInterval = setInterval(async () => {
    if (!currentToken || isReturningToLogin) return;
    try {
      const resp = await fetch(`/api/sessions/${sessionId}/heartbeat`, {
        method: "POST",
        headers: { Authorization: `Bearer ${currentToken}` },
      });
      if (resp.status === 401) {
        const refreshed = await refreshToken();
        if (!refreshed) {
          isReturningToLogin = true;
          stopHeartbeat();
          clearSession();
          ui?.showNotification("Session expired. Please log in again.", "error");
          hideReconnectOverlay();
          handleDisconnect();
          isReturningToLogin = false;
        }
      } else if (resp.status === 404) {
        isReturningToLogin = true;
        stopHeartbeat();
        clearSession();
        ui?.showNotification("Remote session has ended.", "info");
        hideReconnectOverlay();
        handleDisconnect();
        isReturningToLogin = false;
      }
    } catch {
      // Network failure — WebRTC reconnect handles connectivity
    }
  }, 30_000);
}

function stopHeartbeat(): void {
  if (heartbeatInterval) {
    clearInterval(heartbeatInterval);
    heartbeatInterval = null;
  }
}

function startStatsPolling(): void {
  stopStatsPolling();
  statsInterval = setInterval(() => {
    pollWebRTCStats();
  }, 1000);
}

function stopStatsPolling(): void {
  if (statsInterval) {
    clearInterval(statsInterval);
    statsInterval = null;
  }
}

// --- Idle timeout warning ---

/** Record user activity and hide the warning if visible */
function recordActivity(): void {
  lastActivity = Date.now();
  if (idleWarningVisible) {
    hideIdleWarning();
    // Send an immediate heartbeat to reset the server-side idle timer
    // now that the user has returned from being idle.
    sendActivityHeartbeat();
  }
}

/** Send an extra heartbeat after the user returns from idle.
 *  This resets the server-side `last_activity` timestamp immediately
 *  rather than waiting for the next 30s heartbeat tick. */
function sendActivityHeartbeat(): void {
  const session = loadSession();
  if (session && currentToken) {
    fetch(`/api/sessions/${session.session_id}/heartbeat`, {
      method: "POST",
      headers: { Authorization: `Bearer ${currentToken}` },
    }).catch(() => { /* regular heartbeat will retry */ });
  }
}

function showIdleWarning(): void {
  if (idleWarningVisible) return;
  idleWarningVisible = true;
  idleWarning.classList.add("visible");
  console.warn("Idle timeout warning: session will expire soon due to inactivity");
}

function hideIdleWarning(): void {
  if (!idleWarningVisible) return;
  idleWarningVisible = false;
  idleWarning.classList.remove("visible");
}

/** Start periodic idle check. Shows warning when user has been idle
 *  for (idle_timeout - warning_threshold) seconds. */
function startIdleCheck(): void {
  stopIdleCheck();
  lastActivity = Date.now();

  // idle_timeout=0 means disabled on the server — no warning needed
  if (effectiveIdleTimeoutSecs <= 0) return;

  idleCheckInterval = setInterval(() => {
    const idleSecs = (Date.now() - lastActivity) / 1000;
    const warningThreshold = effectiveIdleTimeoutSecs - IDLE_WARNING_BEFORE_SECS;
    if (idleSecs >= warningThreshold) {
      showIdleWarning();
    }
  }, IDLE_CHECK_INTERVAL_MS);
}

function stopIdleCheck(): void {
  if (idleCheckInterval) {
    clearInterval(idleCheckInterval);
    idleCheckInterval = null;
  }
  hideIdleWarning();
}

function handleDisconnect(): void {
  connection?.disconnect();
  connection = null;
  renderer?.destroy();
  renderer = null;
  inputHandler?.disable();
  inputHandler = null;
  clipboardBridge?.disable();
  clipboardBridge = null;
  fileUploader = null;
  fileDownloader = null;
  if (reconnectTimer) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }
  stopStatsPolling();
  stopHeartbeat();
  stopIdleCheck();
  if (refreshTimer) {
    clearTimeout(refreshTimer);
    refreshTimer = null;
  }
  currentToken = null;
  currentReleaseToken = null;
  currentSessionId = null;
  prevBytesReceived = 0;
  prevStatsTimestamp = 0;
  prevPacketsReceived = 0;
  prevPacketsLost = 0;
  sessionBytesReceived = 0;
  prevAudioBytesReceived = 0;
  prevAudioStatsTimestamp = 0;
  prevFramesDecoded = 0;
  prevTotalDecodeTime = 0;
  currentDecodeTimeMs = 0;
  rttSamples = [];
  lastJitterMs = 0;
  lastVideoCodec = "";
  lastVideoResolution = "";
  totalPacketsReceived = 0;
  totalPacketsLost = 0;
  connectedSinceTime = null;
  sessionUsername = null;
  hideSessionInfoPanel();
  hideClipboardHistoryPanel();
  hideAdminPanel();

  // Hide bandwidth indicator and network quality dot
  bandwidthIndicator.classList.remove("visible");
  nqDot.classList.remove("visible");
  qualityScoreHistory = [];

  // Reset latency stats display
  lsRtt.textContent = "RTT: --";
  lsRtt.className = "ls-stat";
  lsFps.textContent = "FPS: --";
  lsDecode.textContent = "Dec: --";
  lsLoss.textContent = "Loss: --";
  lsTooltip.innerHTML = "";

  // Clear saved session
  clearSession();

  hideReconnectOverlay();
  showLogin();
  setStatus("disconnected", "Disconnected");
  ui?.showNotification("Disconnected from remote desktop", "info");

  connectBtn.disabled = false;
  connectBtn.textContent = "Connect";
}

const ICON_WIFI_OFF = `<svg width="32" height="32" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
  <line x1="1" y1="1" x2="23" y2="23"></line>
  <path d="M16.72 11.06A10.94 10.94 0 0 1 19 12.55"></path>
  <path d="M5 12.55a10.94 10.94 0 0 1 5.17-2.39"></path>
  <path d="M10.71 5.05A16 16 0 0 1 22.56 9"></path>
  <path d="M1.42 9a15.91 15.91 0 0 1 4.7-2.88"></path>
  <path d="M8.53 16.11a6 6 0 0 1 6.95 0"></path>
  <line x1="12" y1="20" x2="12.01" y2="20"></line>
</svg>`;

const ICON_TAB = `<svg width="32" height="32" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round">
  <rect x="3" y="3" width="18" height="18" rx="2" ry="2"></rect>
  <rect x="3" y="3" width="9" height="9" rx="1" ry="1" fill="currentColor" opacity="0.2"></rect>
  <line x1="9" y1="3" x2="9" y2="12"></line>
  <line x1="3" y1="12" x2="21" y2="12"></line>
</svg>`;

// Track whether the reconnect overlay is showing an auto-reconnect countdown
// so the reconnect button click can skip the countdown.
let isAutoReconnectCountdown = false;

function showReconnectOverlay(mode: "disconnected" | "replaced" | "auto-reconnecting" = "disconnected", countdownSeconds?: number): void {
  if (mode === "replaced") {
    reconnectIcon.innerHTML = ICON_TAB;
    reconnectTitle.textContent = "Session in use";
    reconnectDesc.textContent = "This session was opened in another tab.";
    reconnectBtn.textContent = "Take back";
    isAutoReconnectCountdown = false;
  } else if (mode === "auto-reconnecting" && countdownSeconds !== undefined && countdownSeconds > 0) {
    reconnectIcon.innerHTML = ICON_WIFI_OFF;
    reconnectTitle.textContent = "Network change detected";
    reconnectDesc.textContent = `Reconnecting in ${countdownSeconds}...`;
    reconnectBtn.textContent = "Reconnect now";
    isAutoReconnectCountdown = true;
  } else {
    reconnectIcon.innerHTML = ICON_WIFI_OFF;
    reconnectTitle.textContent = "Connection lost";
    reconnectDesc.textContent = "Your session is still running on the server.";
    reconnectBtn.textContent = "Reconnect";
    isAutoReconnectCountdown = false;
  }
  reconnectBtn.disabled = false;
  reconnectOverlay.classList.add("visible");
  reconnectBtn.focus();
}

/** Update the reconnect overlay countdown text without resetting focus/layout */
function updateReconnectCountdown(seconds: number): void {
  if (seconds > 0) {
    reconnectDesc.textContent = `Reconnecting in ${seconds}...`;
  } else {
    reconnectTitle.textContent = "Reconnecting...";
    reconnectDesc.textContent = "Re-establishing connection to your session.";
    reconnectBtn.textContent = "Reconnecting...";
    reconnectBtn.disabled = true;
    isAutoReconnectCountdown = false;
  }
}

function hideReconnectOverlay(): void {
  reconnectOverlay.classList.remove("visible");
  isAutoReconnectCountdown = false;
}

/** Attempt to reconnect using the existing session */
async function handleReconnectClick(): Promise<void> {
  if (isReturningToLogin) return;

  const session = loadSession();
  if (!session || !currentToken) {
    handleDisconnect();
    return;
  }

  const defaultLabel = reconnectBtn.textContent || "Reconnect";
  reconnectBtn.disabled = true;
  reconnectBtn.textContent = "Reconnecting...";

  // Try refreshing the token first (it may have expired during the disconnect)
  const refreshed = await refreshToken();
  if (!refreshed) {
    // Token refresh failed — session is likely gone
    reconnectBtn.disabled = false;
    reconnectBtn.textContent = defaultLabel;
    reconnectDesc.textContent = "Session expired. Returning to login...";
    setTimeout(() => handleDisconnect(), 1500);
    return;
  }

  try {
    hideReconnectOverlay();
    setStatus("connecting", "Reconnecting...");
    await startConnection(session.session_id, currentToken!);
  } catch {
    reconnectBtn.disabled = false;
    reconnectBtn.textContent = defaultLabel;
    reconnectDesc.textContent = "Unable to reconnect. Check your network and try again.";
    reconnectOverlay.classList.add("visible");
  }
}

/** End the remote session entirely (kills the agent process on the server) */
function handleEndSession(): void {
  const session = loadSession();
  const token = currentToken;

  // Belt-and-suspenders: send release beacon before the DELETE call.
  // If the DELETE fails (e.g., network issues), the grace period still runs.
  sendReleaseBeacon();

  // Fire DELETE request before handleDisconnect clears the token
  if (session && token) {
    fetch(`/api/sessions/${session.session_id}`, {
      method: "DELETE",
      headers: { Authorization: `Bearer ${token}` },
    }).catch(() => { /* server reaper will clean up eventually */ });
  }

  handleDisconnect();
  ui?.showNotification("Session ended", "info");
}

/** Capture the current video frame and download it as a PNG */
function captureScreenshot(): void {
  const video = renderer?.getVideoElement();
  if (!video || video.videoWidth === 0) return;

  const canvas = document.createElement("canvas");
  canvas.width = video.videoWidth;
  canvas.height = video.videoHeight;
  const ctx = canvas.getContext("2d");
  if (!ctx) return;
  ctx.drawImage(video, 0, 0);

  const link = document.createElement("a");
  link.download = `beam-screenshot-${new Date().toISOString().replace(/[:.]/g, "-")}.png`;
  link.href = canvas.toDataURL("image/png");
  link.click();

  // Brief white flash for visual feedback (camera shutter effect)
  const flash = document.getElementById("screenshot-flash");
  if (flash) {
    flash.classList.add("active");
    setTimeout(() => flash.classList.remove("active"), 200);
  }

  ui?.showNotification("Screenshot saved", "success");
}

function toggleFullscreen(): void {
  if (document.fullscreenElement) {
    renderer?.exitFullscreen();
  } else {
    renderer?.enterFullscreen();
  }
}

/** Update the forward keys button to reflect current state */
function updateForwardKeysButton(enabled: boolean): void {
  btnForwardKeys.textContent = enabled ? "Capturing" : "Capture";
  btnForwardKeys.classList.toggle("active", enabled);
  btnForwardKeys.setAttribute("aria-pressed", String(enabled));
}

/** Toggle forwarding of browser shortcuts to the remote desktop */
function toggleForwardKeys(): void {
  if (!inputHandler) return;
  const enabled = !inputHandler.forwardBrowserShortcuts;
  inputHandler.forwardBrowserShortcuts = enabled;
  localStorage.setItem(FORWARD_KEYS_KEY, enabled ? "true" : "false");
  updateForwardKeysButton(enabled);
}

/** Update the mute button text to reflect current audio state */
function updateMuteButton(muted: boolean): void {
  btnMute.textContent = muted ? "Unmute" : "Mute";
  localStorage.setItem(AUDIO_MUTED_KEY, muted ? "true" : "false");
}

/** Toggle audio mute via the renderer */
function toggleMute(): void {
  if (!renderer) return;
  const muted = renderer.toggleMute();
  updateMuteButton(muted);
}

async function handleLogin(event: SubmitEvent): Promise<void> {
  event.preventDefault();
  hideLoginError();

  const username = usernameInput.value.trim();
  const password = passwordInput.value;

  if (!username || !password) {
    showLoginError("Username and password are required.");
    shakeLoginCard();
    return;
  }

  connectBtn.disabled = true;
  connectBtn.textContent = "Signing in...";
  showLoading("Authenticating...");
  setStatus("connecting", "Authenticating...");

  const MAX_RETRIES = 3;
  const BASE_DELAY = 1000;

  for (let attempt = 0; attempt <= MAX_RETRIES; attempt++) {
    try {
      const response = await fetch("/api/auth/login", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify(Object.assign({
          username,
          password,
          // Subtract the 28px status bar from viewport height so the remote
          // desktop resolution matches the actual video area.
          // Round down to even numbers (H.264 encoders require even dimensions).
          viewport_width: Math.floor(window.innerWidth / 2) * 2,
          viewport_height: Math.floor((window.innerHeight - 28) / 2) * 2,
        }, sessionTimeoutSelect.value ? { idle_timeout: parseInt(sessionTimeoutSelect.value, 10) } : {})),
      });

      if (!response.ok) {
        const text = await response.text();
        let message = "Authentication failed.";
        try {
          const body = JSON.parse(text) as { error?: string };
          if (body.error) message = body.error;
        } catch {
          // Use default message
        }

        // Only retry on 5xx or network errors, not on 4xx (auth failures)
        if (response.status >= 500 && attempt < MAX_RETRIES) {
          const delay = BASE_DELAY * Math.pow(2, attempt);
          updateLoadingStatus(`Retrying (${attempt + 1}/${MAX_RETRIES}) in ${delay}ms...`);
          await new Promise(resolve => setTimeout(resolve, delay));
          continue;
        }

        throw new Error(message);
      }

      const data = (await response.json()) as LoginResponse;

      // Persist session for reconnect on page refresh / browser crash
      saveSession(data);
      localStorage.setItem("beam_username", username);
      // Save timeout selection for next login
      localStorage.setItem(SESSION_TIMEOUT_KEY, sessionTimeoutSelect.value);
      currentToken = data.token;
      currentSessionId = data.session_id;
      currentReleaseToken = data.release_token ?? null;
      sessionUsername = username;
      // Update idle timeout from server response
      if (data.idle_timeout !== undefined) {
        effectiveIdleTimeoutSecs = data.idle_timeout;
      }
      scheduleTokenRefresh();

      updateLoadingStatus("Starting session...");
      await startConnection(data.session_id, data.token);
      return; // Success
    } catch (err) {
      if (attempt < MAX_RETRIES && (!(err instanceof Error) || !err.message.includes("Invalid credentials"))) {
        const delay = BASE_DELAY * Math.pow(2, attempt);
        updateLoadingStatus(`Retrying (${attempt + 1}/${MAX_RETRIES}) after error...`);
        await new Promise(resolve => setTimeout(resolve, delay));
        continue;
      }
      const message = err instanceof Error ? err.message : "Connection failed.";
      showLoadingError(message);
      setStatus("error", message);
      return;
    }
  }
}

async function startConnection(sessionId: string, token: string): Promise<void> {
  // Clean up any existing connection to prevent stale callbacks from
  // overwriting state (e.g., old saved session with invalid token retrying
  // and disabling inputHandler on each disconnect).
  if (connection) {
    connection.disconnect();
    connection = null;
  }
  if (reconnectTimer) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }

  setStatus("connecting", "Connecting...");

  // Timeout: if no video track arrives within 20 seconds, show error
  if (connectionTimeout) clearTimeout(connectionTimeout);
  connectionTimeout = setTimeout(() => {
    if (!renderer?.hasStream()) {
      showLoadingError("Desktop is taking too long to start. Please try again.");
      connection?.disconnect();
      connection = null;
      setStatus("error", "Connection timeout");
    }
    connectionTimeout = null;
  }, 20_000);

  connection = new BeamConnection(sessionId, token);
  renderer = new Renderer(remoteVideo, desktopView);

  // Sync mute button when renderer's mute state changes (e.g. click-to-unmute)
  renderer.onMuteChange((muted) => updateMuteButton(muted));

  // Apply saved audio preference. If the user previously unmuted, the
  // click-to-unmute one-shot in Renderer will also fire on first click,
  // but we can pre-set the state here. Due to browser autoplay policy,
  // unmuting only takes effect after user interaction — the one-shot click
  // handler in Renderer covers that case.
  const savedMuted = localStorage.getItem(AUDIO_MUTED_KEY);
  if (savedMuted === "false") {
    // User previously chose to have audio on. We can't auto-unmute due to
    // autoplay policy, but we update the button to show the intent. The
    // Renderer's click-to-unmute will fire on first interaction.
    updateMuteButton(true); // still muted until interaction
  } else {
    updateMuteButton(true); // default: muted
  }

  // Initialize UI
  ui = new BeamUI();
  ui.setOnFullscreen(toggleFullscreen);
  ui.setOnDisconnect(handleDisconnect);
  ui.setOnEndSession(handleEndSession);

  // Wire FPS updates from renderer to UI + perf overlay
  renderer.onFpsUpdate((fps) => {
    updateLatencyStatsFps(fps);
    perfFps = fps;
  });

  connection.onTrack((stream: MediaStream) => {
    renderer?.attachStream(stream);
    hideReconnectOverlay();
    showDesktop();
    setStatus("connected", "Connected");
    ui?.showNotification("Connected to remote desktop", "success");
    connectedSinceTime = Date.now();
    if (sessionInfoVisible) {
      updateSessionInfoPanel();
      startSessionDurationTimer();
    }
    startStatsPolling();
    startHeartbeat(sessionId);
    startIdleCheck();

    // Notify InputHandler after the first video frame is decoded so it
    // can safely send resize events. Chrome's H.264 decoder can't handle
    // mid-stream resolution changes, so we must wait until the decoder
    // has stabilized on the initial resolution before allowing resizes.
    const video = remoteVideo as HTMLVideoElement & {
      requestVideoFrameCallback?: (cb: () => void) => void;
    };
    const onFirstFrame = () => {
      inputHandler?.notifyFirstFrame();
    };
    if (video.requestVideoFrameCallback) {
      video.requestVideoFrameCallback(onFirstFrame);
    } else {
      // Fallback for browsers without requestVideoFrameCallback
      setTimeout(onFirstFrame, 2000);
    }
  });

  connection.onDataChannelOpen(() => {
    setStatus("connected", "Connected");
    const sendInput = connection!.sendInput.bind(connection!);

    if (!inputHandler) {
      // First connection: set up input capture
      inputHandler = new InputHandler(desktopView, sendInput);
      // Restore forward keys preference
      const savedForwardKeys = localStorage.getItem(FORWARD_KEYS_KEY) === "true";
      inputHandler.forwardBrowserShortcuts = savedForwardKeys;
      updateForwardKeysButton(savedForwardKeys);
      inputHandler.enable();

      // Schedule a WebRTC soft reconnect when a significant resize happens.
      // Chrome's H.264 decoder can't handle mid-stream resolution changes,
      // so we need a fresh PeerConnection after the agent changes xrandr.
      inputHandler.onResizeNeeded(() => {
        if (reconnectTimer) clearTimeout(reconnectTimer);
        reconnectTimer = setTimeout(() => {
          reconnectTimer = null;
          console.log("Triggering soft reconnect after significant resize");
          connection?.softReconnect();
        }, RECONNECT_DELAY_MS);
      });

      // Wire up manual layout selector
      const layoutSelect = document.getElementById("layout-select") as HTMLSelectElement | null;
      if (layoutSelect) {
        layoutSelect.onchange = () => {
          const layout = layoutSelect.value;
          localStorage.setItem("beam_keyboard_layout", layout);
          inputHandler?.sendSpecificLayout(layout);
        };
      }

      // Wire up quality mode selector
      const qualitySelect = document.getElementById("quality-select") as HTMLSelectElement | null;
      if (qualitySelect) {
        qualitySelect.onchange = () => {
          const mode = qualitySelect.value as "auto" | "high" | "low";
          localStorage.setItem(QUALITY_MODE_KEY, mode);
          qualityMode = mode;
          qualityScoreHistory = [];
          if (mode === "auto") {
            // Default to high, let the monitor adjust
            autoQualityLevel = "high";
            connection?.sendInput({ t: "q", mode: "high" });
          } else {
            connection?.sendInput({ t: "q", mode });
          }
          updateQualitySelectDisplay();
        };
      }

      // Wire up scroll speed selector
      const scrollSpeedSelect = document.getElementById("scroll-speed-select") as HTMLSelectElement | null;
      if (scrollSpeedSelect) {
        const savedScrollSpeed = localStorage.getItem(SCROLL_SPEED_KEY);
        if (savedScrollSpeed) {
          scrollSpeedSelect.value = savedScrollSpeed;
          inputHandler?.setScrollMultiplier(parseFloat(savedScrollSpeed));
        }
        scrollSpeedSelect.onchange = () => {
          const speed = scrollSpeedSelect.value;
          localStorage.setItem(SCROLL_SPEED_KEY, speed);
          inputHandler?.setScrollMultiplier(parseFloat(speed));
        };
      }
    }

    // Always re-send layout, quality, and current dimensions on (re)connect.
    // Sending dimensions immediately is critical: the agent starts at Xorg's
    // default resolution (2048x1536), which Chrome's H.264 decoder may fail
    // to decode. Without an immediate resize, no frames decode, so
    // requestVideoFrameCallback never fires, and we're stuck forever.
    inputHandler.sendLayout();
    inputHandler.sendCurrentDimensions();
    const savedQuality = localStorage.getItem(QUALITY_MODE_KEY) || "auto";
    qualityMode = savedQuality as "auto" | "high" | "low";
    const effectiveQuality = qualityMode === "auto" ? autoQualityLevel : qualityMode;
    sendInput({ t: "q", mode: effectiveQuality });
    updateQualitySelectDisplay();

    if (!fileUploader) {
      fileUploader = new FileUploader(sendInput);
      fileUploader.setProgressCallback((filename, percent) => {
        if (percent >= 100) {
          ui?.showNotification(`Uploaded: ${filename}`, "success");
        }
      });
    }

    if (!fileDownloader) {
      fileDownloader = new FileDownloader();
      fileDownloader.setCompleteCallback((filename) => {
        ui?.showNotification(`Downloaded: ${filename}`, "success");
      });
      fileDownloader.setErrorCallback((error) => {
        ui?.showNotification(`Download failed: ${error}`, "error");
      });
    }

    if (!clipboardBridge) {
      clipboardBridge = new ClipboardBridge(sendInput);
      clipboardBridge.onClipboardSync((direction, preview) => {
        const label = direction === "sent" ? "Clipboard sent" : "Clipboard received";
        const message = preview ? `${label}: ${preview}` : label;
        ui?.showNotification(message, "info", 2000);
      });
      clipboardBridge.onHistoryChange(() => {
        renderClipboardHistory();
      });
    }
    clipboardBridge.enable();
  });

  // Handle messages from agent (clipboard sync, cursor shape, file download)
  connection.onDataChannelMessage((msg) => {
    if (msg.t === "c" && "text" in msg) {
      clipboardBridge?.handleRemoteClipboard(msg.text);
    }
    if (msg.t === "cur" && "css" in msg) {
      remoteVideo.style.cursor = msg.css;
    }
    if (msg.t === "fds" || msg.t === "fdc" || msg.t === "fdd" || msg.t === "fde") {
      fileDownloader?.handleMessage(msg as DownloadMessage);
    }
  });

  connection.onDisconnect(() => {
    setStatus("connecting", "Reconnecting...");
    ui?.showNotification("Connection lost, reconnecting...", "error");
    inputHandler?.disable();
    inputHandler = null;
    clipboardBridge?.disable();
    clipboardBridge = null;
    stopStatsPolling();
    stopHeartbeat();
    stopIdleCheck();
  });

  connection.onReconnecting((attempt, max) => {
    setStatus("connecting", `Reconnecting (${attempt}/${max})...`);
  });

  connection.onReconnectFailed(() => {
    setStatus("error", "Connection lost");
    ui?.showNotification("Connection lost. Click Reconnect to try again.", "error");
    // Show reconnect overlay instead of going back to login.
    // Keep token and session intact so user can reconnect without re-login.
    showReconnectOverlay();
    // Restart heartbeat — onDisconnect already stopped it, but we need it
    // to detect if the server-side session dies while the overlay is shown.
    const session = loadSession();
    if (session) {
      startHeartbeat(session.session_id);
    }
  });

  // Auto-reconnect countdown: ICE detected a network change (disconnected/failed).
  // Show the overlay with a countdown so the user knows what's happening.
  // They can click "Reconnect now" to skip the countdown, or wait for auto.
  connection.onAutoReconnecting((secondsRemaining) => {
    if (secondsRemaining > 0) {
      // First callback (e.g. 3): show the overlay with countdown
      if (!reconnectOverlay.classList.contains("visible") || !isAutoReconnectCountdown) {
        setStatus("connecting", "Network change detected");
        showReconnectOverlay("auto-reconnecting", secondsRemaining);
      } else {
        // Subsequent callbacks (2, 1): just update the countdown text
        updateReconnectCountdown(secondsRemaining);
      }
    } else {
      // 0: auto-reconnect is now triggering
      updateReconnectCountdown(0);
    }
  });

  // ICE self-recovered during countdown: cancel the overlay
  connection.onAutoReconnectRecovered(() => {
    if (isAutoReconnectCountdown) {
      hideReconnectOverlay();
      setStatus("connected", "Connected");
      ui?.showNotification("Connection recovered", "success");
    }
  });

  connection.onAgentExited(() => {
    setStatus("error", "Session ended unexpectedly");
    ui?.showNotification("Your remote desktop session ended unexpectedly.", "error");
    clearSession();
    handleDisconnect();
  });

  connection.onReplaced(() => {
    setStatus("error", "Connected from another tab");
    // Clean up resources but keep session/token so user can take it back
    renderer?.destroy();
    renderer = null;
    inputHandler?.disable();
    inputHandler = null;
    clipboardBridge?.disable();
    clipboardBridge = null;
    stopStatsPolling();
    stopHeartbeat();
    connection = null;
    showReconnectOverlay("replaced");
  });

  await connection.connect();
}

// F1 help overlay, F8 mute toggle, F9 performance overlay, F11 fullscreen, F12 screenshot
document.addEventListener("keydown", (e: KeyboardEvent) => {
  if (e.key === "F1") {
    e.preventDefault();
    helpOverlay.classList.toggle("visible");
  }
  if (e.key === "F7") {
    e.preventDefault();
    toggleAdminPanel();
  }
  if (e.key === "F8") {
    e.preventDefault();
    toggleMute();
  }
  if (e.key === "F11") {
    e.preventDefault();
    toggleFullscreen();
  }
  if (e.key === "F9") {
    e.preventDefault();
    perfOverlay.classList.toggle("visible");
  }
  if (e.key === "F10") {
    e.preventDefault();
    toggleSessionInfoPanel();
  }
  if (e.key === "F12") {
    e.preventDefault();
    captureScreenshot();
  }
  // Ctrl+Shift+V: toggle clipboard history panel
  if (e.key === "V" && e.ctrlKey && e.shiftKey) {
    e.preventDefault();
    toggleClipboardHistoryPanel();
  }
});

// Listen for login form submission
loginForm.addEventListener("submit", (e: SubmitEvent) => {
  handleLogin(e);
});

// Password show/hide toggle
passwordToggle.addEventListener("click", () => {
  const isPassword = passwordInput.type === "password";
  passwordInput.type = isPassword ? "text" : "password";
  passwordToggle.textContent = isPassword ? "Hide" : "Show";
  passwordToggle.setAttribute("aria-label", isPassword ? "Hide password" : "Show password");
});

// Reconnect overlay buttons
reconnectBtn.addEventListener("click", () => {
  handleReconnectClick();
});
reconnectDisconnectBtn.addEventListener("click", () => {
  hideReconnectOverlay();
  handleDisconnect();
});

// Session info panel close button
sipCloseBtn.addEventListener("click", () => {
  hideSessionInfoPanel();
});

// Clipboard history panel buttons
chpCloseBtn.addEventListener("click", () => {
  hideClipboardHistoryPanel();
});
chpClearBtn.addEventListener("click", () => {
  clipboardBridge?.clearHistory();
  renderClipboardHistory();
});

// Admin panel close button + click-outside-to-close
adminPanelClose.addEventListener("click", () => {
  hideAdminPanel();
});
adminPanelOverlay.addEventListener("click", (e) => {
  if (e.target === adminPanelOverlay) {
    hideAdminPanel();
  }
});

// Session info panel copy stats button
const sipCopyStatsBtn = document.getElementById("sip-copy-stats") as HTMLButtonElement;
sipCopyStatsBtn.addEventListener("click", () => {
  copyStatsToClipboard();
});

// Forward browser shortcuts toggle
btnForwardKeys.addEventListener("click", () => {
  toggleForwardKeys();
});

// Mute/unmute button
btnMute.addEventListener("click", () => {
  toggleMute();
});

// Theme toggle button
btnTheme.addEventListener("click", () => {
  toggleTheme();
});

// Cancel button during loading
loadingCancel.addEventListener("click", () => {
  if (connectionTimeout) {
    clearTimeout(connectionTimeout);
    connectionTimeout = null;
  }
  connection?.disconnect();
  connection = null;
  hideLoading();
  setStatus("disconnected", "Disconnected");
});

// Track user activity for idle timeout warning.
// These fire on any mouse/keyboard interaction in the desktop view,
// resetting the idle timer. The listeners are always attached but
// recordActivity() is a no-op when no session is active (idleCheckInterval
// is null, so the warning never shows).
desktopView.addEventListener("mousemove", recordActivity);
desktopView.addEventListener("mousedown", recordActivity);
desktopView.addEventListener("wheel", recordActivity);
document.addEventListener("keydown", recordActivity);

// Graceful session release on tab/window close. sendBeacon() is reliable
// during unload (unlike fetch), and the server starts a 60s grace period.
// If the user returns (page refresh, back button), the session reconnects
// and cancels the grace period.
window.addEventListener("beforeunload", () => {
  sendReleaseBeacon();
});

// When the tab becomes visible after being backgrounded, fire an immediate
// heartbeat. Browsers throttle timers in background tabs, so the regular
// 30s heartbeat may have been delayed for minutes. An immediate heartbeat
// resets the server-side idle timer and detects if the session was reaped.
//
// Also notify the agent of tab visibility changes so it can reduce capture
// framerate when the tab is backgrounded (saves GPU/CPU/bandwidth).
document.addEventListener("visibilitychange", () => {
  const visible = document.visibilityState === "visible";

  // Send visibility state to agent via DataChannel
  if (connection) {
    console.debug(`Tab visibility changed: ${visible ? "visible" : "hidden"}`);
    connection.sendInput({ t: "vs", visible });
  }

  if (visible && currentToken && heartbeatInterval) {
    const session = loadSession();
    if (session) {
      fetch(`/api/sessions/${session.session_id}/heartbeat`, {
        method: "POST",
        headers: { Authorization: `Bearer ${currentToken}` },
      }).catch(() => { /* handled by regular heartbeat */ });
    }
  }
});

// --- File upload: drag-and-drop + button ---

const fileDropOverlay = document.getElementById("file-drop-overlay") as HTMLDivElement;
const btnUpload = document.getElementById("btn-upload") as HTMLButtonElement;
const fileUploadInput = document.getElementById("file-upload-input") as HTMLInputElement;
let dragCounter = 0;

desktopView.addEventListener("dragenter", (e: DragEvent) => {
  e.preventDefault();
  dragCounter++;
  if (dragCounter === 1) {
    fileDropOverlay.classList.add("visible");
  }
});

desktopView.addEventListener("dragleave", (e: DragEvent) => {
  e.preventDefault();
  dragCounter--;
  if (dragCounter <= 0) {
    dragCounter = 0;
    fileDropOverlay.classList.remove("visible");
  }
});

desktopView.addEventListener("dragover", (e: DragEvent) => {
  e.preventDefault();
});

desktopView.addEventListener("drop", (e: DragEvent) => {
  e.preventDefault();
  dragCounter = 0;
  fileDropOverlay.classList.remove("visible");

  const files = e.dataTransfer?.files;
  if (files && fileUploader) {
    for (let i = 0; i < files.length; i++) {
      const file = files[i];
      ui?.showNotification(`Uploading: ${file.name}`, "info", 2000);
      fileUploader.uploadFile(file).catch((err) => {
        ui?.showNotification(`Upload failed: ${file.name}`, "error");
        console.error("File upload error:", err);
      });
    }
  }
});

btnUpload.addEventListener("click", () => {
  fileUploadInput.click();
});

fileUploadInput.addEventListener("change", () => {
  const files = fileUploadInput.files;
  if (files && fileUploader) {
    for (let i = 0; i < files.length; i++) {
      const file = files[i];
      ui?.showNotification(`Uploading: ${file.name}`, "info", 2000);
      fileUploader.uploadFile(file).catch((err) => {
        ui?.showNotification(`Upload failed: ${file.name}`, "error");
        console.error("File upload error:", err);
      });
    }
  }
  // Reset input so the same file can be uploaded again
  fileUploadInput.value = "";
});

// --- File download button ---

const btnDownload = document.getElementById("btn-download") as HTMLButtonElement;
btnDownload.addEventListener("click", () => {
  const path = window.prompt("Enter file path on remote desktop (relative to home or absolute):");
  if (path && connection) {
    ui?.showNotification(`Requesting download: ${path}`, "info", 2000);
    connection.sendInput({ t: "fdr", path } as import("./connection").InputEvent);
  }
});

// --- Mobile FAB and virtual keyboard ---

const isTouchDevice = "ontouchstart" in window || navigator.maxTouchPoints > 0;
const mobileFab = document.getElementById("mobile-fab") as HTMLDivElement;
const mobileFabToggle = document.getElementById("mobile-fab-toggle") as HTMLButtonElement;
const mobileFabMenu = document.getElementById("mobile-fab-menu") as HTMLDivElement;
const fabKeyboard = document.getElementById("fab-keyboard") as HTMLButtonElement;
const fabFullscreen = document.getElementById("fab-fullscreen") as HTMLButtonElement;
const fabScreenshot = document.getElementById("fab-screenshot") as HTMLButtonElement;
const fabDisconnect = document.getElementById("fab-disconnect") as HTMLButtonElement;
const mobileKeyboardInput = document.getElementById("mobile-keyboard-input") as HTMLInputElement;

let fabOpen = false;

function toggleFab(): void {
  fabOpen = !fabOpen;
  mobileFabToggle.classList.toggle("open", fabOpen);
  mobileFabMenu.classList.toggle("visible", fabOpen);
  mobileFabToggle.setAttribute("aria-expanded", String(fabOpen));
}

function closeFab(): void {
  fabOpen = false;
  mobileFabToggle.classList.remove("open");
  mobileFabMenu.classList.remove("visible");
  mobileFabToggle.setAttribute("aria-expanded", "false");
}

mobileFabToggle.addEventListener("click", (e) => {
  e.stopPropagation();
  toggleFab();
});

fabKeyboard.addEventListener("click", () => {
  closeFab();
  mobileKeyboardInput.focus();
});

fabFullscreen.addEventListener("click", () => {
  closeFab();
  toggleFullscreen();
});

fabScreenshot.addEventListener("click", () => {
  closeFab();
  captureScreenshot();
});

fabDisconnect.addEventListener("click", () => {
  closeFab();
  handleDisconnect();
});

// Close FAB menu when tapping outside
document.addEventListener("click", (e) => {
  if (fabOpen && !mobileFab.contains(e.target as Node)) {
    closeFab();
  }
});

// Virtual keyboard: forward key events from the hidden input to the remote
mobileKeyboardInput.addEventListener("input", () => {
  const text = mobileKeyboardInput.value;
  if (text && connection) {
    // Send each character as a clipboard-paste event for reliability.
    // Mobile keyboards produce composed text, not individual key codes,
    // so we send the text directly via the clipboard input event.
    connection.sendInput({ t: "c", text });
  }
  // Clear the input for the next character
  mobileKeyboardInput.value = "";
});

// Handle Enter key from virtual keyboard
mobileKeyboardInput.addEventListener("keydown", (e: KeyboardEvent) => {
  if (e.key === "Enter") {
    e.preventDefault();
    // Send Enter key (evdev code 28)
    if (connection) {
      connection.sendInput({ t: "k", c: 28, d: true });
      connection.sendInput({ t: "k", c: 28, d: false });
    }
  } else if (e.key === "Backspace") {
    e.preventDefault();
    // Send Backspace key (evdev code 14)
    if (connection) {
      connection.sendInput({ t: "k", c: 14, d: true });
      connection.sendInput({ t: "k", c: 14, d: false });
    }
  } else if (e.key === "Escape") {
    e.preventDefault();
    // Send Escape key (evdev code 1)
    if (connection) {
      connection.sendInput({ t: "k", c: 1, d: true });
      connection.sendInput({ t: "k", c: 1, d: false });
    }
    mobileKeyboardInput.blur();
  } else if (e.key === "Tab") {
    e.preventDefault();
    // Send Tab key (evdev code 15)
    if (connection) {
      connection.sendInput({ t: "k", c: 15, d: true });
      connection.sendInput({ t: "k", c: 15, d: false });
    }
  }
});

// Track touch events for idle timeout
if (isTouchDevice) {
  desktopView.addEventListener("touchstart", recordActivity);
  desktopView.addEventListener("touchmove", recordActivity);
}

// Pre-fill username from last successful login
const savedUsername = localStorage.getItem("beam_username");
if (savedUsername) {
  usernameInput.value = savedUsername;
  passwordInput.focus();
}

// Restore saved quality mode selection
const savedQualityMode = localStorage.getItem(QUALITY_MODE_KEY);
if (savedQualityMode) {
  qualityMode = savedQualityMode as "auto" | "high" | "low";
  const qualitySelect = document.getElementById("quality-select") as HTMLSelectElement | null;
  if (qualitySelect) {
    qualitySelect.value = savedQualityMode;
  }
  updateQualitySelectDisplay();
}

// Restore saved session timeout selection
const savedTimeout = localStorage.getItem(SESSION_TIMEOUT_KEY);
if (savedTimeout !== null) {
  sessionTimeoutSelect.value = savedTimeout;
}

// Attempt to resume previous session on page load
const savedSession = loadSession();
if (savedSession) {
  (async () => {
    try {
      // First verify the session exists on the server to avoid stuck "reconnecting" state
      const resp = await fetch("/api/sessions", {
        headers: { Authorization: `Bearer ${savedSession.token}` },
      });

      if (!resp.ok) {
        throw new Error("Session invalid");
      }

      const sessions = await resp.json() as { id: string }[];
      if (!sessions.some(s => s.id === savedSession.session_id)) {
        throw new Error("Session not found on server");
      }

      currentToken = savedSession.token;
      currentSessionId = savedSession.session_id;
      currentReleaseToken = savedSession.release_token ?? null;
      sessionUsername = localStorage.getItem("beam_username");
      if (savedSession.idle_timeout !== undefined) {
        effectiveIdleTimeoutSecs = savedSession.idle_timeout;
      }
      scheduleTokenRefresh();
      showLoading("Resuming session...");
      startConnection(savedSession.session_id, savedSession.token);
    } catch (err) {
      console.warn("Could not resume previous session:", err);
      clearSession();
      showLogin();
    }
  })();
}
