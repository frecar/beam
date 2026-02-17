/**
 * DOM element references, visibility toggling (show/hide login,
 * show/hide desktop view, loading states), status management,
 * and reconnect overlay.
 */

// --- DOM element references ---

export const loginView = document.getElementById("login-view") as HTMLDivElement;
export const desktopView = document.getElementById("desktop-view") as HTMLDivElement;
export const loginForm = document.getElementById("login-form") as HTMLFormElement;
export const usernameInput = document.getElementById("username") as HTMLInputElement;
export const passwordInput = document.getElementById("password") as HTMLInputElement;
export const connectBtn = document.getElementById("connect-btn") as HTMLButtonElement;
export const loginError = document.getElementById("login-error") as HTMLDivElement;
export const loginCard = document.querySelector(".login-card") as HTMLDivElement;
export const passwordToggle = document.getElementById("password-toggle") as HTMLButtonElement;
export const sessionTimeoutSelect = document.getElementById("session-timeout") as HTMLSelectElement;
export const loginFormContent = document.getElementById("login-form-content") as HTMLDivElement;
export const loginLoading = document.getElementById("login-loading") as HTMLDivElement;
export const loadingSpinner = document.getElementById("loading-spinner") as HTMLDivElement;
export const loadingStatus = document.getElementById("loading-status") as HTMLParagraphElement;
export const loadingCancel = document.getElementById("loading-cancel") as HTMLButtonElement;
export const remoteVideo = document.getElementById("remote-video") as HTMLVideoElement;
export const statusBar = document.getElementById("status-bar") as HTMLDivElement;
export const statusDot = document.getElementById("status-dot") as HTMLDivElement;
export const statusText = document.getElementById("status-text") as HTMLSpanElement;
export const statusVersion = document.getElementById("status-version") as HTMLSpanElement;

export const bandwidthIndicator = document.getElementById("bandwidth-indicator") as HTMLSpanElement;
export const lsRtt = document.getElementById("ls-rtt") as HTMLSpanElement;
export const lsFps = document.getElementById("ls-fps") as HTMLSpanElement;
export const lsDecode = document.getElementById("ls-decode") as HTMLSpanElement;
export const lsLoss = document.getElementById("ls-loss") as HTMLSpanElement;
export const lsTooltip = document.getElementById("ls-tooltip") as HTMLDivElement;
export const faviconLink = document.querySelector("link[rel='icon']") as HTMLLinkElement;

export const btnMute = document.getElementById("btn-mute") as HTMLButtonElement;
export const btnForwardKeys = document.getElementById("btn-forward-keys") as HTMLButtonElement;
export const btnTheme = document.getElementById("btn-theme") as HTMLButtonElement;
export const perfOverlay = document.getElementById("perf-overlay") as HTMLDivElement;
export const helpOverlay = document.getElementById("help-overlay") as HTMLDivElement;
export const sessionInfoPanel = document.getElementById("session-info-panel") as HTMLDivElement;
export const sipCloseBtn = document.getElementById("sip-close") as HTMLButtonElement;
export const reconnectOverlay = document.getElementById("reconnect-overlay") as HTMLDivElement;
export const reconnectTitle = document.getElementById("reconnect-title") as HTMLHeadingElement;
export const reconnectIcon = document.querySelector(".reconnect-icon") as HTMLDivElement;
export const reconnectBtn = document.getElementById("reconnect-btn") as HTMLButtonElement;
export const reconnectDisconnectBtn = document.getElementById("reconnect-disconnect-btn") as HTMLButtonElement;
export const reconnectDesc = document.getElementById("reconnect-desc") as HTMLParagraphElement;
export const idleWarning = document.getElementById("idle-warning") as HTMLDivElement;
export const nqDot = document.getElementById("nq-dot") as HTMLSpanElement;

// Clipboard history panel
export const clipboardHistoryPanel = document.getElementById("clipboard-history-panel") as HTMLDivElement;
export const chpList = document.getElementById("chp-list") as HTMLDivElement;
export const chpClearBtn = document.getElementById("chp-clear") as HTMLButtonElement;
export const chpCloseBtn = document.getElementById("chp-close") as HTMLButtonElement;

// Admin sessions panel
export const adminPanelOverlay = document.getElementById("admin-panel-overlay") as HTMLDivElement;
export const adminSessionsTbody = document.getElementById("admin-sessions-tbody") as HTMLTableSectionElement;
export const adminSessionCount = document.getElementById("admin-session-count") as HTMLSpanElement;
export const adminPanelClose = document.getElementById("admin-panel-close") as HTMLButtonElement;

// File upload/download
export const fileDropOverlay = document.getElementById("file-drop-overlay") as HTMLDivElement;
export const btnUpload = document.getElementById("btn-upload") as HTMLButtonElement;
export const fileUploadInput = document.getElementById("file-upload-input") as HTMLInputElement;
export const btnDownload = document.getElementById("btn-download") as HTMLButtonElement;

// Mobile FAB
export const mobileFab = document.getElementById("mobile-fab") as HTMLDivElement;
export const mobileFabToggle = document.getElementById("mobile-fab-toggle") as HTMLButtonElement;
export const mobileFabMenu = document.getElementById("mobile-fab-menu") as HTMLDivElement;
export const fabKeyboard = document.getElementById("fab-keyboard") as HTMLButtonElement;
export const fabFullscreen = document.getElementById("fab-fullscreen") as HTMLButtonElement;
export const fabScreenshot = document.getElementById("fab-screenshot") as HTMLButtonElement;
export const fabDisconnect = document.getElementById("fab-disconnect") as HTMLButtonElement;
export const mobileKeyboardInput = document.getElementById("mobile-keyboard-input") as HTMLInputElement;

export const sipCopyStatsBtn = document.getElementById("sip-copy-stats") as HTMLButtonElement;

// --- Connection state type ---
export type ConnectionState = "disconnected" | "connecting" | "connected" | "error";

// --- Favicon ---

/** Generate an SVG data URL for a colored circle favicon */
function faviconDataUrl(color: string): string {
  return `data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 16 16'%3E%3Ccircle cx='8' cy='8' r='7' fill='${encodeURIComponent(color)}'/%3E%3C/svg%3E`;
}

export function setFavicon(color: string): void {
  if (faviconLink) {
    faviconLink.href = faviconDataUrl(color);
  }
}

// --- Status management ---

export function setStatus(
  state: ConnectionState,
  message: string,
  onStateChange?: (state: ConnectionState) => void,
): void {
  onStateChange?.(state);
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
export function updateConnectionQuality(rttMs: number, currentConnectionState: ConnectionState): void {
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

// --- Loading states ---

export function showLoginError(message: string): void {
  loginError.textContent = message;
  loginError.style.display = "block";
}

export function hideLoginError(): void {
  loginError.style.display = "none";
}

export function showLoading(message: string): void {
  hideLoginError();
  loginFormContent.style.display = "none";
  loginLoading.style.display = "flex";
  loadingSpinner.className = "loading-spinner";
  loadingStatus.className = "loading-status";
  loadingStatus.textContent = message;
  loadingCancel.textContent = "Cancel";
  // Move focus to cancel button so keyboard users can act on the loading state
  loadingCancel.focus();
}

export function updateLoadingStatus(message: string): void {
  loadingStatus.style.opacity = "0";
  setTimeout(() => {
    loadingStatus.textContent = message;
    loadingStatus.style.opacity = "1";
  }, 150);
}

export function shakeLoginCard(): void {
  loginCard.classList.remove("shake");
  // Force reflow so re-adding the class restarts the animation
  void loginCard.offsetWidth;
  loginCard.classList.add("shake");
  loginCard.addEventListener("animationend", () => loginCard.classList.remove("shake"), { once: true });
}

export function showLoadingError(message: string): void {
  loadingSpinner.classList.add("error");
  loadingStatus.textContent = message;
  loadingStatus.classList.add("error");
  shakeLoginCard();
  loadingCancel.textContent = "Back to login";
  // Move focus to the action button so keyboard users know what to do next
  loadingCancel.focus();
}

export function hideLoading(): void {
  loginLoading.style.display = "none";
  loginFormContent.style.display = "block";
  loadingSpinner.className = "loading-spinner";
  loadingStatus.className = "loading-status";
  loadingCancel.textContent = "Cancel";
  connectBtn.disabled = false;
  connectBtn.textContent = "Sign in";
  // Return focus to username input so keyboard users land back on the form
  usernameInput.focus();
}

// --- View switching ---

export function showDesktop(
  isTouchDevice: boolean,
  connectionTimeout: ReturnType<typeof setTimeout> | null,
  clearConnectionTimeout: () => void,
): void {
  loginView.style.display = "none";
  desktopView.style.display = "block";
  statusBar.classList.add("visible");
  if (isTouchDevice) {
    mobileFab.classList.add("visible");
  }
  if (connectionTimeout) {
    clearConnectionTimeout();
  }
  // Fetch and display server version (best-effort, non-blocking)
  fetch("/api/health")
    .then((r) => r.json())
    .then((data: { version?: string }) => {
      if (data.version) statusVersion.textContent = `v${data.version}`;
    })
    .catch(() => {});
}

export function showLogin(closeFab: () => void): void {
  loginView.style.display = "flex";
  desktopView.style.display = "none";
  statusBar.classList.remove("visible");
  mobileFab.classList.remove("visible");
  closeFab();
  hideLoading();
}

// --- Reconnect overlay ---

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
export let isAutoReconnectCountdown = false;

export function showReconnectOverlay(mode: "disconnected" | "replaced" | "auto-reconnecting" = "disconnected", countdownSeconds?: number): void {
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
export function updateReconnectCountdown(seconds: number): void {
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

export function hideReconnectOverlay(): void {
  reconnectOverlay.classList.remove("visible");
  isAutoReconnectCountdown = false;
}
