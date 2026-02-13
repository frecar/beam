import { ClipboardBridge } from "./clipboard";
import { BeamConnection } from "./connection";
import { InputHandler } from "./input";
import { Renderer } from "./renderer";
import { BeamUI } from "./ui";

/** Shape of the login API response */
interface LoginResponse {
  session_id: string;
  token: string;
}

/** Stored session with expiry timestamp */
interface StoredSession extends LoginResponse {
  saved_at: number;
}

const SESSION_KEY = "beam_session";
const SESSION_MAX_AGE_MS = 3600_000; // 1 hour — matches server reaper

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
const loginFormContent = document.getElementById("login-form-content") as HTMLDivElement;
const loginLoading = document.getElementById("login-loading") as HTMLDivElement;
const loadingSpinner = document.getElementById("loading-spinner") as HTMLDivElement;
const loadingStatus = document.getElementById("loading-status") as HTMLParagraphElement;
const loadingCancel = document.getElementById("loading-cancel") as HTMLButtonElement;
const remoteVideo = document.getElementById("remote-video") as HTMLVideoElement;
const statusBar = document.getElementById("status-bar") as HTMLDivElement;
const statusDot = document.getElementById("status-dot") as HTMLDivElement;
const statusText = document.getElementById("status-text") as HTMLSpanElement;

const perfOverlay = document.getElementById("perf-overlay") as HTMLDivElement;
const reconnectOverlay = document.getElementById("reconnect-overlay") as HTMLDivElement;
const reconnectTitle = document.getElementById("reconnect-title") as HTMLHeadingElement;
const reconnectIcon = document.querySelector(".reconnect-icon") as HTMLDivElement;
const reconnectBtn = document.getElementById("reconnect-btn") as HTMLButtonElement;
const reconnectDisconnectBtn = document.getElementById("reconnect-disconnect-btn") as HTMLButtonElement;
const reconnectDesc = document.getElementById("reconnect-desc") as HTMLParagraphElement;

let connection: BeamConnection | null = null;
let renderer: Renderer | null = null;
let inputHandler: InputHandler | null = null;
let clipboardBridge: ClipboardBridge | null = null;
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

// Guard against race between heartbeat 404 and user clicking reconnect
let isReturningToLogin = false;

// For calculating received video bitrate from inbound-rtp stats
let prevBytesReceived = 0;
let prevStatsTimestamp = 0;

// Performance overlay state (updated from stats poll + renderer)
let perfFps = 0;
let perfLatency = 0;
let perfBitrate = 0;
let perfLoss = 0;

function setStatus(state: "disconnected" | "connecting" | "connected" | "error", message: string): void {
  statusText.textContent = message;
  statusDot.className = "status-dot";

  switch (state) {
    case "connected":
      statusDot.classList.add("connected");
      break;
    case "connecting":
      statusDot.classList.add("connecting");
      break;
    case "error":
      statusDot.classList.add("error");
      break;
  }
}

function setToken(token: string): void {
  currentToken = token;
  const data = loadSession();
  if (data) {
    data.token = token;
    saveSession(data);
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

function showLoadingError(message: string): void {
  loadingSpinner.classList.add("error");
  loadingStatus.textContent = message;
  loadingStatus.classList.add("error");
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
  if (connectionTimeout) {
    clearTimeout(connectionTimeout);
    connectionTimeout = null;
  }
}

function showLogin(): void {
  loginView.style.display = "flex";
  desktopView.style.display = "none";
  statusBar.classList.remove("visible");
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
  let candidateType = "";
  let currentBytesReceived = 0;
  let currentTimestamp = 0;

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
    }

    // Detect connection type (relay = TURN, srflx = STUN, host = direct)
    if (report.type === "local-candidate" && report.isRemote === false) {
      if (report.candidateType) {
        candidateType = report.candidateType;
      }
    }
  });

  // Calculate actual received video bitrate from inbound-rtp delta
  if (currentBytesReceived > 0 && prevBytesReceived > 0 && currentTimestamp > prevStatsTimestamp) {
    const deltaBytes = currentBytesReceived - prevBytesReceived;
    const deltaSec = (currentTimestamp - prevStatsTimestamp) / 1000;
    if (deltaSec > 0) {
      bitrateKbps = Math.round((deltaBytes * 8) / deltaSec / 1000);
    }
  }
  prevBytesReceived = currentBytesReceived;
  prevStatsTimestamp = currentTimestamp;

  if (rttMs !== null) {
    ui?.updateLatency(rttMs);
  }

  // Calculate loss percentage
  const lossPercent =
    packetsReceived > 0 ? ((packetsLost / packetsReceived) * 100).toFixed(1) : "0.0";

  // Update quality info in toolbar
  ui?.updateQuality(bitrateKbps, parseFloat(lossPercent), candidateType);

  // Update performance overlay state
  if (rttMs !== null) perfLatency = rttMs;
  if (bitrateKbps !== null) perfBitrate = bitrateKbps;
  perfLoss = parseFloat(lossPercent);
  updatePerfOverlay();

  // Warn if video element has no frames decoded yet (debugging aid)
  if (remoteVideo.srcObject && remoteVideo.videoWidth === 0 && remoteVideo.videoHeight === 0) {
    console.warn("Video element has srcObject but 0x0 dimensions - no frames decoded yet");
  }
}

/** Update the performance overlay content with color-coded values */
function updatePerfOverlay(): void {
  if (!perfOverlay.classList.contains("visible")) return;

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
  }, 2000);
}

function stopStatsPolling(): void {
  if (statsInterval) {
    clearInterval(statsInterval);
    statsInterval = null;
  }
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
  if (reconnectTimer) {
    clearTimeout(reconnectTimer);
    reconnectTimer = null;
  }
  stopStatsPolling();
  stopHeartbeat();
  if (refreshTimer) {
    clearTimeout(refreshTimer);
    refreshTimer = null;
  }
  currentToken = null;

  // Clear saved session
  clearSession();

  hideReconnectOverlay();
  showLogin();
  setStatus("disconnected", "Disconnected");
  document.title = "Beam";
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

function showReconnectOverlay(mode: "disconnected" | "replaced" = "disconnected"): void {
  if (mode === "replaced") {
    reconnectIcon.innerHTML = ICON_TAB;
    reconnectTitle.textContent = "Session in use";
    reconnectDesc.textContent = "This session was opened in another tab.";
    reconnectBtn.textContent = "Take back";
  } else {
    reconnectIcon.innerHTML = ICON_WIFI_OFF;
    reconnectTitle.textContent = "Connection lost";
    reconnectDesc.textContent = "Your session is still running on the server.";
    reconnectBtn.textContent = "Reconnect";
  }
  reconnectBtn.disabled = false;
  reconnectOverlay.classList.add("visible");
  reconnectBtn.focus();
}

function hideReconnectOverlay(): void {
  reconnectOverlay.classList.remove("visible");
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

function toggleFullscreen(): void {
  if (document.fullscreenElement) {
    renderer?.exitFullscreen();
  } else {
    renderer?.enterFullscreen();
  }
}

async function handleLogin(event: SubmitEvent): Promise<void> {
  event.preventDefault();
  hideLoginError();

  const username = usernameInput.value.trim();
  const password = passwordInput.value;

  if (!username || !password) {
    showLoginError("Username and password are required.");
    return;
  }

  showLoading("Authenticating...");
  setStatus("connecting", "Authenticating...");

  try {
    const response = await fetch("/api/auth/login", {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        username,
        password,
        // Subtract the 28px status bar from viewport height so the remote
        // desktop resolution matches the actual video area.
        // Round down to even numbers (H.264 encoders require even dimensions).
        viewport_width: Math.floor(window.innerWidth / 2) * 2,
        viewport_height: Math.floor((window.innerHeight - 28) / 2) * 2,
      }),
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
      throw new Error(message);
    }

    const data = (await response.json()) as LoginResponse;

    // Persist session for reconnect on page refresh / browser crash
    saveSession(data);
    currentToken = data.token;
    scheduleTokenRefresh();

    updateLoadingStatus("Starting session...");
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
  renderer = new Renderer(remoteVideo, desktopView);

  // Initialize UI
  ui = new BeamUI();
  ui.setOnFullscreen(toggleFullscreen);
  ui.setOnDisconnect(handleDisconnect);
  ui.setOnEndSession(handleEndSession);

  // Wire FPS updates from renderer to UI + perf overlay
  renderer.onFpsUpdate((fps) => {
    ui?.updateFps(fps);
    perfFps = fps;
  });

  connection.onTrack((stream: MediaStream) => {
    renderer?.attachStream(stream);
    hideReconnectOverlay();
    showDesktop();
    setStatus("connected", "Connected");
    document.title = "Beam - Connected";
    ui?.showNotification("Connected to remote desktop", "success");
    startStatsPolling();
    startHeartbeat(sessionId);

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
          const mode = qualitySelect.value;
          localStorage.setItem("beam_quality_mode", mode);
          connection?.sendInput({ t: "q", mode });
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
    const savedQuality = localStorage.getItem("beam_quality_mode") || "high";
    sendInput({ t: "q", mode: savedQuality });

    if (!clipboardBridge) {
      clipboardBridge = new ClipboardBridge(sendInput);
    }
    clipboardBridge.enable();
  });

  // Handle messages from agent (clipboard sync, cursor shape)
  connection.onDataChannelMessage((msg) => {
    if (msg.t === "c" && "text" in msg) {
      clipboardBridge?.handleRemoteClipboard(msg.text);
    }
    if (msg.t === "cur" && "css" in msg) {
      remoteVideo.style.cursor = msg.css;
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

// F11 keyboard shortcut for fullscreen, F9 for performance overlay
document.addEventListener("keydown", (e: KeyboardEvent) => {
  if (e.key === "F11") {
    e.preventDefault();
    toggleFullscreen();
  }
  if (e.key === "F9") {
    e.preventDefault();
    perfOverlay.classList.toggle("visible");
  }
});

// Listen for login form submission
loginForm.addEventListener("submit", (e: SubmitEvent) => {
  handleLogin(e);
});

// Reconnect overlay buttons
reconnectBtn.addEventListener("click", () => {
  handleReconnectClick();
});
reconnectDisconnectBtn.addEventListener("click", () => {
  hideReconnectOverlay();
  handleDisconnect();
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

// When the tab becomes visible after being backgrounded, fire an immediate
// heartbeat. Browsers throttle timers in background tabs, so the regular
// 30s heartbeat may have been delayed for minutes. An immediate heartbeat
// resets the server-side idle timer and detects if the session was reaped.
document.addEventListener("visibilitychange", () => {
  if (document.visibilityState === "visible" && currentToken && heartbeatInterval) {
    const session = loadSession();
    if (session) {
      fetch(`/api/sessions/${session.session_id}/heartbeat`, {
        method: "POST",
        headers: { Authorization: `Bearer ${currentToken}` },
      }).catch(() => { /* handled by regular heartbeat */ });
    }
  }
});

// Attempt to resume previous session on page load
const savedSession = loadSession();
if (savedSession) {
  try {
    currentToken = savedSession.token;
    scheduleTokenRefresh();
    showLoading("Reconnecting...");
    startConnection(savedSession.session_id, savedSession.token);
  } catch {
    clearSession();
  }
}
