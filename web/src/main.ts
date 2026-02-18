import { ClipboardBridge, type ClipboardHistoryEntry } from "./clipboard";
import { BeamConnection } from "./connection";
import { FileDownloader, FileUploader } from "./filetransfer";
import type { DownloadMessage } from "./filetransfer";
import { InputHandler } from "./input";
import { performLogin, clearRateLimitTimer } from "./login";
import { WebCodecsRenderer } from "./webcodecs-renderer";
import {
  loadSession, clearSession, sendReleaseBeacon, TokenManager,
} from "./session";
import {
  initTheme, toggleTheme, updateThemeButton,
  THEME_KEY, AUDIO_MUTED_KEY, SCROLL_SPEED_KEY,
  FORWARD_KEYS_KEY, SESSION_TIMEOUT_KEY,
  IDLE_WARNING_BEFORE_SECS, IDLE_CHECK_INTERVAL_MS,
  updatePerfOverlay,
  updateLatencyStatsFps,
  showIdleWarning, hideIdleWarning,
  resetLatencyStats, resetNetworkIndicators,
} from "./settings";
import { BeamUI } from "./ui";
import {
  type ConnectionState,
  loginForm, usernameInput, passwordInput, connectBtn,
  passwordToggle, sessionTimeoutSelect,
  loadingCancel, remoteCanvas, desktopView,
  helpOverlay, perfOverlay, sessionInfoPanel, sipCloseBtn,
  reconnectBtn, reconnectDisconnectBtn, reconnectOverlay,
  clipboardHistoryPanel, chpList, chpClearBtn, chpCloseBtn,
  adminPanelOverlay, adminSessionsTbody, adminSessionCount, adminPanelClose,
  fileDropOverlay, btnUpload, fileUploadInput, btnDownload,
  mobileFab, mobileFabToggle, mobileFabMenu,
  fabKeyboard, fabFullscreen, fabScreenshot, fabDisconnect,
  mobileKeyboardInput, sipCopyStatsBtn,
  btnMute, btnForwardKeys, btnTheme,
  setStatus as setStatusUI,
  showLoading, hideLoading, showLoadingError,
  showDesktop as showDesktopUI, showLogin as showLoginUI,
  showReconnectOverlay, hideReconnectOverlay,
  reconnectDesc,
} from "./ui-state";

// --- Token manager (singleton) ---
const tokenManager = new TokenManager();

// Idle timeout warning: updated from the login response idle_timeout field.
// We warn 2 minutes before expiry.
let effectiveIdleTimeoutSecs = 3600; // updated from login response

let connection: BeamConnection | null = null;
let renderer: WebCodecsRenderer | null = null;
let inputHandler: InputHandler | null = null;
let clipboardBridge: ClipboardBridge | null = null;
let fileUploader: FileUploader | null = null;
let fileDownloader: FileDownloader | null = null;
let ui: BeamUI | null = null;
let heartbeatInterval: ReturnType<typeof setInterval> | null = null;
let connectionTimeout: ReturnType<typeof setTimeout> | null = null;

// Release token for graceful session cleanup on tab close
let currentReleaseToken: string | null = null;
let currentSessionId: string | null = null;

// Guard against race between heartbeat 404 and user clicking reconnect
let isReturningToLogin = false;

// Performance overlay state (updated from renderer)
let perfFps = 0;


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

function setStatus(state: ConnectionState, message: string): void {
  setStatusUI(state, message);
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

  // Update video stats from renderer
  const setText = (id: string, text: string) => {
    const el = document.getElementById(id);
    if (el) el.textContent = text;
  };

  if (renderer) {
    const w = renderer.getVideoWidth();
    const h = renderer.getVideoHeight();
    if (w > 0 && h > 0) {
      setText("sip-resolution", `${w}x${h}`);
    }
    setText("sip-framerate", `${renderer.getFps()} fps`);
    setText("sip-video-codec", "H.264");
  }

  // Transport is now WebSocket
  setText("sip-ice-state", "N/A (WebSocket)");
  setText("sip-transport", "WSS");
  setText("sip-local-candidate", "N/A");
  setText("sip-remote-candidate", "N/A");
  setText("sip-dtls-state", "N/A");

  // Audio muted state
  const sipAudioMuted = document.getElementById("sip-audio-muted");
  if (sipAudioMuted && renderer) {
    sipAudioMuted.textContent = renderer.isMuted() ? "Yes" : "No";
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

  // Video
  const resolution = getText("sip-resolution");
  const framerate = getText("sip-framerate");
  const videoCodec = getText("sip-video-codec");

  // Audio
  const audioCodec = getText("sip-audio-codec");
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
    `  Transport: WebSocket`,
    "",
    "Video:",
    `  Resolution: ${resolution}`,
    `  Framerate: ${framerate}`,
    `  Codec: ${videoCodec}`,
    "",
    "Audio:",
    `  Codec: ${audioCodec}`,
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
      // Network failure -- WS reconnect handles connectivity
    }
  }, 30_000);
}

function stopHeartbeat(): void {
  if (heartbeatInterval) {
    clearInterval(heartbeatInterval);
    heartbeatInterval = null;
  }
}

// --- Idle timeout warning ---

/** Record user activity and hide the warning if visible */
function recordActivity(): void {
  lastActivity = Date.now();
  if (idleWarningVisible) {
    idleWarningVisible = hideIdleWarning(idleWarningVisible);
    sendActivityHeartbeat();
  }
}

/** Send an extra heartbeat after the user returns from idle. */
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
  stopHeartbeat();
  stopIdleCheck();
  tokenManager.clearToken();
  currentReleaseToken = null;
  currentSessionId = null;
  connectedSinceTime = null;
  sessionUsername = null;
  hideSessionInfoPanel();
  hideClipboardHistoryPanel();
  hideAdminPanel();

  // Reset indicators
  resetNetworkIndicators();
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

  const refreshed = await tokenManager.refreshToken();
  if (!refreshed) {
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

  sendReleaseBeacon(currentSessionId, currentReleaseToken);

  if (session && token) {
    fetch(`/api/sessions/${session.session_id}`, {
      method: "DELETE",
      headers: { Authorization: `Bearer ${token}` },
    }).catch(() => { /* server reaper will clean up eventually */ });
  }

  handleDisconnect();
  ui?.showNotification("Session ended", "info");
}

/** Capture the current canvas frame and download it as a PNG */
function captureScreenshot(): void {
  const canvas = renderer?.getCanvas();
  if (!canvas || renderer!.getVideoWidth() === 0) return;

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

  tokenManager.setToken(data.token);
  tokenManager.setConnection(connection);
  currentSessionId = data.session_id;
  currentReleaseToken = data.release_token ?? null;
  sessionUsername = usernameInput.value.trim();
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
  if (connection) {
    connection.disconnect();
    connection = null;
  }

  setStatus("connecting", "Connecting...");

  // Timeout: if no video frame arrives within 20 seconds, show error
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
  renderer = new WebCodecsRenderer(remoteCanvas, desktopView);

  // Sync mute button when renderer's mute state changes (e.g. click-to-unmute)
  renderer.onMuteChange((muted) => updateMuteButton(muted));

  // Restore saved audio preference — if user previously unmuted, attempt to
  // resume AudioContext. Browsers allow AudioContext.resume() after any prior
  // user gesture in this origin, so this succeeds for returning users.
  const savedMuted = localStorage.getItem(AUDIO_MUTED_KEY) !== "false";
  updateMuteButton(savedMuted);
  if (!savedMuted) {
    renderer.setAudioMuted(false);
  }

  // Initialize UI
  ui = new BeamUI();
  ui.setOnFullscreen(toggleFullscreen);
  ui.setOnDisconnect(handleDisconnect);
  ui.setOnEndSession(handleEndSession);

  // Wire FPS + decode time updates from renderer to UI + perf overlay
  renderer.onFpsUpdate((fps, decodeMs) => {
    updateLatencyStatsFps(fps, decodeMs);
    perfFps = fps;
    updatePerfOverlay(decodeMs, perfFps, 0, 0);
  });

  // Wire video frames from connection to renderer
  connection.onVideoFrame((flags, width, height, timestampUs, payload) => {
    renderer?.feedVideoFrame(flags, width, height, timestampUs, payload);
  });

  // Wire audio frames from connection to renderer
  connection.onAudioFrame((timestampUs, payload) => {
    renderer?.feedAudioFrame(timestampUs, payload);
  });

  // Notify when first video frame is decoded
  renderer.onFirstFrame(() => {
    hideReconnectOverlay();
    showDesktop();
    setStatus("connected", "Connected");
    ui?.showNotification("Connected to remote desktop", "success");
    connectedSinceTime = Date.now();
    if (sessionInfoVisible) {
      updateSessionInfoPanel();
      startSessionDurationTimer();
    }
    startHeartbeat(sessionId);
    startIdleCheck();
    inputHandler?.notifyFirstFrame();
  });

  // When WS connection opens, set up input and features
  connection.onConnected(() => {
    setStatus("connected", "Connected");
    const sendInput = connection!.sendInput.bind(connection!);

    if (!inputHandler) {
      inputHandler = new InputHandler(desktopView, sendInput);
      const savedForwardKeys = localStorage.getItem(FORWARD_KEYS_KEY) === "true";
      inputHandler.forwardBrowserShortcuts = savedForwardKeys;
      updateForwardKeysButton(savedForwardKeys);
      inputHandler.enable();

      // No soft reconnect needed in WebCodecs mode -- the decoder handles
      // resolution changes inline via reconfiguration.

      // Wire up manual layout selector
      const layoutSelect = document.getElementById("layout-select") as HTMLSelectElement | null;
      if (layoutSelect) {
        layoutSelect.onchange = () => {
          const layout = layoutSelect.value;
          localStorage.setItem("beam_keyboard_layout", layout);
          inputHandler?.sendSpecificLayout(layout);
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

    // Re-send layout and current dimensions on (re)connect
    inputHandler.sendLayout();
    inputHandler.sendCurrentDimensions();

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
  connection.onAgentMessage((msg) => {
    if (msg.t === "c" && "text" in msg) {
      clipboardBridge?.handleRemoteClipboard(msg.text);
    }
    if (msg.t === "cur" && "css" in msg) {
      remoteCanvas.style.cursor = msg.css;
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
    stopHeartbeat();
    stopIdleCheck();
  });

  connection.onReconnecting((attempt, max) => {
    setStatus("connecting", `Reconnecting (${attempt}/${max})...`);
  });

  connection.onReconnectFailed(() => {
    setStatus("error", "Connection lost");
    ui?.showNotification("Connection lost. Click Reconnect to try again.", "error");
    showReconnectOverlay();
    const session = loadSession();
    if (session) {
      startHeartbeat(session.session_id);
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
    renderer?.destroy();
    renderer = null;
    inputHandler?.disable();
    inputHandler = null;
    clipboardBridge?.disable();
    clipboardBridge = null;
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
  clearRateLimitTimer();
  hideLoading();
  setStatus("disconnected", "Disconnected");
});

// Track user activity for idle timeout warning
desktopView.addEventListener("mousemove", recordActivity);
desktopView.addEventListener("mousedown", recordActivity);
desktopView.addEventListener("wheel", recordActivity, { passive: true });
document.addEventListener("keydown", recordActivity);

// Graceful session release on tab/window close
window.addEventListener("beforeunload", () => {
  sendReleaseBeacon(currentSessionId, currentReleaseToken);
});

// Tab visibility changes: send heartbeat + notify agent
document.addEventListener("visibilitychange", () => {
  const visible = document.visibilityState === "visible";

  // Send visibility state to agent via WS
  if (connection) {
    console.debug(`Tab visibility changed: ${visible ? "visible" : "hidden"}`);
    connection.sendInput({ t: "vs", visible });
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
    connection.sendInput({ t: "c", text });
  }
  mobileKeyboardInput.value = "";
});

// Handle Enter key from virtual keyboard
mobileKeyboardInput.addEventListener("keydown", (e: KeyboardEvent) => {
  if (e.key === "Enter") {
    e.preventDefault();
    if (connection) {
      connection.sendInput({ t: "k", c: 28, d: true });
      connection.sendInput({ t: "k", c: 28, d: false });
    }
  } else if (e.key === "Backspace") {
    e.preventDefault();
    if (connection) {
      connection.sendInput({ t: "k", c: 14, d: true });
      connection.sendInput({ t: "k", c: 14, d: false });
    }
  } else if (e.key === "Escape") {
    e.preventDefault();
    if (connection) {
      connection.sendInput({ t: "k", c: 1, d: true });
      connection.sendInput({ t: "k", c: 1, d: false });
    }
    mobileKeyboardInput.blur();
  } else if (e.key === "Tab") {
    e.preventDefault();
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
