import { ClipboardBridge, type ClipboardHistoryEntry } from "./clipboard";
import { BeamConnection } from "./connection";
import { FileDownloader, FileUploader } from "./filetransfer";
import type { DownloadMessage } from "./filetransfer";
import { InputHandler } from "./input";
import { performLogin, clearRateLimitTimer } from "./login";
import { Renderer } from "./renderer";
import {
  loadSession, clearSession, sendReleaseBeacon, TokenManager,
} from "./session";
import {
  initTheme, toggleTheme, updateThemeButton,
  THEME_KEY, AUDIO_MUTED_KEY, SCROLL_SPEED_KEY,
  FORWARD_KEYS_KEY, QUALITY_MODE_KEY, SESSION_TIMEOUT_KEY,
  IDLE_WARNING_BEFORE_SECS, IDLE_CHECK_INTERVAL_MS,
  computeNetworkScore, updateNetworkQualityDot,
  updateQualitySelectDisplay, switchAutoQuality,
  updateBandwidthIndicator, updatePerfOverlay,
  updateLatencyStats, updateLatencyStatsFps,
  showIdleWarning, hideIdleWarning,
  resetLatencyStats, resetNetworkIndicators,
} from "./settings";
import { BeamUI } from "./ui";
import {
  type ConnectionState,
  loginForm, usernameInput, passwordInput, connectBtn,
  passwordToggle, sessionTimeoutSelect,
  loadingCancel, remoteVideo, desktopView,
  helpOverlay, perfOverlay, sessionInfoPanel, sipCloseBtn,
  reconnectBtn, reconnectDisconnectBtn, reconnectOverlay,
  clipboardHistoryPanel, chpList, chpClearBtn, chpCloseBtn,
  adminPanelOverlay, adminSessionsTbody, adminSessionCount, adminPanelClose,
  fileDropOverlay, btnUpload, fileUploadInput, btnDownload,
  mobileFab, mobileFabToggle, mobileFabMenu,
  fabKeyboard, fabFullscreen, fabScreenshot, fabDisconnect,
  mobileKeyboardInput, sipCopyStatsBtn,
  btnMute, btnForwardKeys, btnTheme,
  setStatus as setStatusUI, updateConnectionQuality,
  showLoading, hideLoading, showLoadingError,
  showDesktop as showDesktopUI, showLogin as showLoginUI,
  showReconnectOverlay, updateReconnectCountdown, hideReconnectOverlay,
  isAutoReconnectCountdown, reconnectDesc,
} from "./ui-state";

// --- Token manager (singleton) ---
const tokenManager = new TokenManager();

// Idle timeout warning: updated from the login response idle_timeout field.
// We warn 2 minutes before expiry.
let effectiveIdleTimeoutSecs = 3600; // updated from login response

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

// Soft reconnect scheduling for resolution changes
let reconnectTimer: ReturnType<typeof setTimeout> | null = null;
const RECONNECT_DELAY_MS = 1000; // Give agent time to process resize

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

// Post-background recovery: detect broken RTP pipeline after extended background.
// Chrome's jitter buffer / H.264 decoder can corrupt after 30+ seconds backgrounded.
// Packets arrive (packetsReceived increments) but 0 frames decode. Soft reconnect
// creates a fresh peer connection — the only reliable fix.
let backgroundedAt: number | null = null;
let postBackgroundRecoveryChecks = 0;
const BACKGROUND_CORRUPTION_THRESHOLD_MS = 30_000;
const POST_BACKGROUND_RECOVERY_POLLS = 3;

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
let currentConnectionState: ConnectionState = "disconnected";

// --- Auto quality mode ---
let qualityMode: "auto" | "high" | "low" = "auto";
let autoQualityLevel: "high" | "low" = "high";
let qualityScoreHistory: { score: number; time: number }[] = [];

// Idle timeout warning state
let lastActivity = Date.now();
let idleCheckInterval: ReturnType<typeof setInterval> | null = null;
let idleWarningVisible = false;

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

// Admin sessions panel state
let adminPanelVisible = false;
let adminRefreshInterval: ReturnType<typeof setInterval> | null = null;

// Session info panel state
let sessionInfoVisible = false;
let connectedSinceTime: number | null = null;
let sessionDurationInterval: ReturnType<typeof setInterval> | null = null;
let sessionUsername: string | null = null;

// Audio stats tracking for session info panel
let prevAudioBytesReceived = 0;
let prevAudioStatsTimestamp = 0;

// --- Wrapper for setStatus that tracks connection state ---
function setStatus(state: ConnectionState, message: string): void {
  setStatusUI(state, message, (s) => { currentConnectionState = s; });
}

// --- Network quality monitor ---

/** Feed stats to the network quality monitor (called from pollWebRTCStats) */
function updateNetworkQualityMonitor(rttMs: number | null, lossPercent: number): void {
  const score = computeNetworkScore(rttMs, lossPercent);
  const now = Date.now();

  // Always update the dot (visible regardless of auto mode)
  updateNetworkQualityDot(score, currentConnectionState);

  if (qualityMode !== "auto") return;

  qualityScoreHistory.push({ score, time: now });
  // Keep last 15 seconds of history
  qualityScoreHistory = qualityScoreHistory.filter(s => now - s.time < 15_000);

  if (autoQualityLevel === "high") {
    // Drop to low: score < 40 sustained for 5 seconds
    const fiveSecsAgo = now - 5_000;
    const recent = qualityScoreHistory.filter(s => s.time >= fiveSecsAgo);
    if (recent.length >= 3 && recent.every(s => s.score < 40)) {
      const result = switchAutoQuality("low", autoQualityLevel, qualityMode, connection, ui);
      autoQualityLevel = result.newLevel;
    }
  } else {
    // Restore to high: score > 70 sustained for 10 seconds
    const tenSecsAgo = now - 10_000;
    const recent = qualityScoreHistory.filter(s => s.time >= tenSecsAgo);
    if (recent.length >= 5 && recent.every(s => s.score > 70)) {
      const result = switchAutoQuality("high", autoQualityLevel, qualityMode, connection, ui);
      autoQualityLevel = result.newLevel;
    }
  }
}

function showDesktop(): void {
  showDesktopUI(isTouchDevice, connectionTimeout, () => {
    if (connectionTimeout) {
      clearTimeout(connectionTimeout);
      connectionTimeout = null;
    }
  });
}

function showLogin(): void {
  showLoginUI(closeFab);
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
  let inboundVideoFps = 0;

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
      if (typeof report.framesPerSecond === "number") {
        inboundVideoFps = report.framesPerSecond;
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
  updateLatencyStats(rttMs, currentDecodeTimeMs, cumulativeLossPercent,
    rttSamples, perfFps, lastJitterMs, totalPacketsReceived, totalPacketsLost,
    lastVideoResolution, lastVideoCodec);

  // Update performance overlay state
  if (rttMs !== null) perfLatency = rttMs;
  if (bitrateKbps !== null) perfBitrate = bitrateKbps;
  perfLoss = Math.round(cumulativeLossPercent * 10) / 10;
  updatePerfOverlay(perfLatency, perfFps, perfBitrate, perfLoss);

  // Update status bar connection quality indicator based on latency
  if (rttMs !== null) updateConnectionQuality(rttMs, currentConnectionState);

  // Update bandwidth indicator in status bar
  updateBandwidthIndicator(bitrateKbps, sessionBytesReceived, currentConnectionState);

  // Feed the network quality monitor with interval loss
  updateNetworkQualityMonitor(rttMs, intervalLossPercent);

  // Warn if video element has no frames decoded yet (debugging aid)
  if (remoteVideo.srcObject && remoteVideo.videoWidth === 0 && remoteVideo.videoHeight === 0) {
    console.warn("Video element has srcObject but 0x0 dimensions - no frames decoded yet");
  }

  // Post-background recovery: if tab was backgrounded 30+ seconds and FPS stays
  // at 0 while packets are arriving, the RTP pipeline is corrupted. Trigger a
  // soft reconnect to get a fresh peer connection (the only reliable fix).
  if (postBackgroundRecoveryChecks > 0) {
    if (inboundVideoFps === 0 && deltaReceived > 10) {
      postBackgroundRecoveryChecks--;
      if (postBackgroundRecoveryChecks === 0) {
        console.warn(
          `Post-background recovery: 0 FPS but ${deltaReceived} packets/s received. ` +
          `RTP pipeline corrupted — triggering soft reconnect.`
        );
        connection?.softReconnect();
      }
    } else if (inboundVideoFps > 0) {
      console.log("Post-background pipeline recovered naturally");
      postBackgroundRecoveryChecks = 0;
    }
  }

  // Update session info panel if visible (reuses the same 2s polling interval)
  updateSessionInfoStats();
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
  const currentToken = tokenManager.getToken();
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

  const currentToken = tokenManager.getToken();
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
    const currentToken = tokenManager.getToken();
    if (!currentToken || isReturningToLogin) return;
    try {
      const resp = await fetch(`/api/sessions/${sessionId}/heartbeat`, {
        method: "POST",
        headers: { Authorization: `Bearer ${currentToken}` },
      });
      if (resp.status === 401) {
        const refreshed = await tokenManager.refreshToken();
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
      // Network failure -- WebRTC reconnect handles connectivity
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
    idleWarningVisible = hideIdleWarning(idleWarningVisible);
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
  const currentToken = tokenManager.getToken();
  if (session && currentToken) {
    fetch(`/api/sessions/${session.session_id}/heartbeat`, {
      method: "POST",
      headers: { Authorization: `Bearer ${currentToken}` },
    }).catch(() => { /* regular heartbeat will retry */ });
  }
}

/** Start periodic idle check. Shows warning when user has been idle
 *  for (idle_timeout - warning_threshold) seconds. */
function startIdleCheck(): void {
  stopIdleCheck();
  lastActivity = Date.now();

  // idle_timeout=0 means disabled on the server -- no warning needed
  if (effectiveIdleTimeoutSecs <= 0) return;

  idleCheckInterval = setInterval(() => {
    const idleSecs = (Date.now() - lastActivity) / 1000;
    const warningThreshold = effectiveIdleTimeoutSecs - IDLE_WARNING_BEFORE_SECS;
    if (idleSecs >= warningThreshold) {
      idleWarningVisible = showIdleWarning(idleWarningVisible);
    }
  }, IDLE_CHECK_INTERVAL_MS);
}

function stopIdleCheck(): void {
  if (idleCheckInterval) {
    clearInterval(idleCheckInterval);
    idleCheckInterval = null;
  }
  idleWarningVisible = hideIdleWarning(idleWarningVisible);
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
  tokenManager.clearToken();
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
  backgroundedAt = null;
  postBackgroundRecoveryChecks = 0;
  connectedSinceTime = null;
  sessionUsername = null;
  hideSessionInfoPanel();
  hideClipboardHistoryPanel();
  hideAdminPanel();

  // Hide bandwidth indicator and network quality dot
  resetNetworkIndicators();
  qualityScoreHistory = [];

  // Reset latency stats display
  resetLatencyStats();

  // Clear saved session
  clearSession();

  hideReconnectOverlay();
  showLogin();
  setStatus("disconnected", "Disconnected");
  ui?.showNotification("Disconnected from remote desktop", "info");

  connectBtn.disabled = false;
  connectBtn.textContent = "Sign in";
}

/** Attempt to reconnect using the existing session */
async function handleReconnectClick(): Promise<void> {
  if (isReturningToLogin) return;

  const session = loadSession();
  const currentToken = tokenManager.getToken();
  if (!session || !currentToken) {
    handleDisconnect();
    return;
  }

  const defaultLabel = reconnectBtn.textContent || "Reconnect";
  reconnectBtn.disabled = true;
  reconnectBtn.textContent = "Reconnecting...";

  // Try refreshing the token first (it may have expired during the disconnect)
  const refreshed = await tokenManager.refreshToken();
  if (!refreshed) {
    // Token refresh failed -- session is likely gone
    reconnectBtn.disabled = false;
    reconnectBtn.textContent = defaultLabel;
    reconnectDesc.textContent = "Session expired. Returning to login...";
    setTimeout(() => handleDisconnect(), 1500);
    return;
  }

  try {
    hideReconnectOverlay();
    setStatus("connecting", "Reconnecting...");
    await startConnection(session.session_id, tokenManager.getToken()!);
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
  const token = tokenManager.getToken();

  // Belt-and-suspenders: send release beacon before the DELETE call.
  // If the DELETE fails (e.g., network issues), the grace period still runs.
  sendReleaseBeacon(currentSessionId, currentReleaseToken);

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

  const data = await performLogin(setStatus);
  if (!data) return;

  // Set up token and session state
  tokenManager.setToken(data.token);
  tokenManager.setConnection(connection);
  currentSessionId = data.session_id;
  currentReleaseToken = data.release_token ?? null;
  sessionUsername = usernameInput.value.trim();
  // Update idle timeout from server response
  if (data.idle_timeout !== undefined) {
    effectiveIdleTimeoutSecs = data.idle_timeout;
  }
  tokenManager.scheduleTokenRefresh();

  try {
    await startConnection(data.session_id, data.token);
  } catch (err) {
    const message = err instanceof Error ? err.message : "Connection failed.";
    showLoadingError(message);
    setStatus("error", message);
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
  tokenManager.setConnection(connection);
  renderer = new Renderer(remoteVideo, desktopView);

  // Sync mute button when renderer's mute state changes (e.g. click-to-unmute)
  renderer.onMuteChange((muted) => updateMuteButton(muted));

  // Apply saved audio preference. If the user previously unmuted, the
  // click-to-unmute one-shot in Renderer will also fire on first click,
  // but we can pre-set the state here. Due to browser autoplay policy,
  // unmuting only takes effect after user interaction -- the one-shot click
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
          updateQualitySelectDisplay(qualityMode, autoQualityLevel);
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
    updateQualitySelectDisplay(qualityMode, autoQualityLevel);

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
    // Restart heartbeat -- onDisconnect already stopped it, but we need it
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

// --- Global keyboard shortcuts ---
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

// --- Event listeners ---

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
  // Clear any running rate-limit countdown
  clearRateLimitTimer();
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
  sendReleaseBeacon(currentSessionId, currentReleaseToken);
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

  // Track background duration for post-background RTP pipeline recovery.
  if (!visible) {
    backgroundedAt = Date.now();
    postBackgroundRecoveryChecks = 0;
  } else if (backgroundedAt && (Date.now() - backgroundedAt) >= BACKGROUND_CORRUPTION_THRESHOLD_MS) {
    // Tab was backgrounded long enough to potentially corrupt the RTP pipeline.
    // Start monitoring FPS in the stats polling loop for recovery failure.
    postBackgroundRecoveryChecks = POST_BACKGROUND_RECOVERY_POLLS;
    console.log(
      `Tab foregrounded after ${Math.round((Date.now() - backgroundedAt) / 1000)}s ` +
      `background, monitoring for pipeline recovery`
    );
    backgroundedAt = null;
  } else {
    backgroundedAt = null; // Short background, no concern
  }

  const currentToken = tokenManager.getToken();
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

btnDownload.addEventListener("click", () => {
  const path = window.prompt("Enter file path on remote desktop (relative to home or absolute):");
  if (path && connection) {
    ui?.showNotification(`Requesting download: ${path}`, "info", 2000);
    connection.sendInput({ t: "fdr", path } as import("./connection").InputEvent);
  }
});

// --- Mobile FAB and virtual keyboard ---

const isTouchDevice = "ontouchstart" in window || navigator.maxTouchPoints > 0;

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

// --- Initialization ---

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
  updateQualitySelectDisplay(qualityMode, autoQualityLevel);
}

// Restore saved session timeout selection
const savedTimeout = localStorage.getItem(SESSION_TIMEOUT_KEY);
if (savedTimeout !== null) {
  sessionTimeoutSelect.value = savedTimeout;
}

// Fetch server version for login screen
fetch("/api/health").then(r => r.json()).then((data: { version?: string }) => {
  const el = document.getElementById("version-footer");
  if (el && data.version) el.textContent = `v${data.version}`;
}).catch(() => { /* silently ignore */ });

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

      tokenManager.setToken(savedSession.token);
      currentSessionId = savedSession.session_id;
      currentReleaseToken = savedSession.release_token ?? null;
      sessionUsername = localStorage.getItem("beam_username");
      if (savedSession.idle_timeout !== undefined) {
        effectiveIdleTimeoutSecs = savedSession.idle_timeout;
      }
      tokenManager.scheduleTokenRefresh();
      showLoading("Resuming session...");
      startConnection(savedSession.session_id, savedSession.token);
    } catch (err) {
      console.warn("Could not resume previous session:", err);
      clearSession();
      showLogin();
    }
  })();
}
