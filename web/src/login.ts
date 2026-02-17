/**
 * Login form handling, rate limit countdown, error display, and focus management.
 */

import type { LoginResponse } from "./session";
import { saveSession } from "./session";
import type { ConnectionState } from "./ui-state";
import {
  usernameInput, connectBtn, loginError, sessionTimeoutSelect,
} from "./ui-state";
import {
  showLoginError, hideLoginError, showLoading,
  updateLoadingStatus, shakeLoginCard, showLoadingError,
  hideLoading,
} from "./ui-state";
import { SESSION_TIMEOUT_KEY } from "./settings";

/** Live countdown for rate-limit lockout timer handle */
let rateLimitTimer: ReturnType<typeof setInterval> | null = null;
/** Client-side login failure counter for progressive warnings (no server oracle) */
let loginFailureCount = 0;

/** Clear any running rate-limit countdown (called from hideLoading in ui-state,
 *  but we also need it here for the login module's own cleanup) */
export function clearRateLimitTimer(): void {
  if (rateLimitTimer) {
    clearInterval(rateLimitTimer);
    rateLimitTimer = null;
    loginError.setAttribute("aria-live", "assertive");
  }
}

/** Live countdown for rate-limit lockout. Updates every second and
 *  disables the submit button until the timer expires. */
export function startRateLimitCountdown(seconds: number): void {
  clearRateLimitTimer();

  let remaining = seconds;
  connectBtn.disabled = true;

  // First announcement is assertive (role="alert" on loginError)
  showLoginError(`Too many attempts. Try again in ${remaining} second${remaining === 1 ? "" : "s"}.`);

  // Subsequent updates: switch to polite so screen readers don't
  // announce every single tick of the countdown
  loginError.setAttribute("aria-live", "polite");

  rateLimitTimer = setInterval(() => {
    remaining--;
    if (remaining <= 0) {
      clearInterval(rateLimitTimer!);
      rateLimitTimer = null;
      loginError.style.display = "none";
      // Restore assertive for future errors
      loginError.setAttribute("aria-live", "assertive");
      connectBtn.disabled = false;
      usernameInput.focus();
    } else {
      loginError.textContent = `Too many attempts. Try again in ${remaining} second${remaining === 1 ? "" : "s"}.`;
    }
  }, 1000);
}

/** Perform the login API call and handle all response scenarios.
 *  Returns the LoginResponse on success, or null on failure. */
export async function performLogin(
  setStatus: (state: ConnectionState, message: string) => void,
): Promise<LoginResponse | null> {
  hideLoginError();

  const username = usernameInput.value.trim();
  const password = (document.getElementById("password") as HTMLInputElement).value;

  if (!username || !password) {
    showLoginError("Username and password are required.");
    shakeLoginCard();
    return null;
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

        // 429: rate limited -- return to login form with assertive alert + countdown
        if (response.status === 429) {
          const retryHeader = response.headers.get("Retry-After");
          const retryAfter = retryHeader ? parseInt(retryHeader, 10) : undefined;
          hideLoading();
          shakeLoginCard();
          if (retryAfter && retryAfter > 0) {
            startRateLimitCountdown(retryAfter);
          } else {
            showLoginError(message);
          }
          setStatus("error", "Rate limited");
          loginFailureCount = 0; // Reset -- server is now tracking
          return null;
        }

        // Client-side progressive warning (no server-side oracle)
        if (response.status === 401) {
          loginFailureCount++;
          hideLoading();
          shakeLoginCard();
          if (loginFailureCount >= 3) {
            showLoginError(`${message} Multiple failed attempts detected.`);
          } else {
            showLoginError(message);
          }
          setStatus("error", message);
          return null;
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
      loginFailureCount = 0; // Reset on success

      // Persist session for reconnect on page refresh / browser crash
      saveSession(data);
      localStorage.setItem("beam_username", username);
      // Save timeout selection for next login
      localStorage.setItem(SESSION_TIMEOUT_KEY, sessionTimeoutSelect.value);

      updateLoadingStatus("Starting session...");
      return data;
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
      return null;
    }
  }
  return null;
}
