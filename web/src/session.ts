/**
 * Session management: localStorage token storage, session creation,
 * token refresh, login/logout API calls, and release beacon.
 */

import type { BeamConnection } from "./connection";

/** Shape of the login API response */
export interface LoginResponse {
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
const SESSION_MAX_AGE_MS = 3600_000; // 1 hour -- matches server reaper

export function saveSession(data: LoginResponse): void {
  const stored: StoredSession = { ...data, saved_at: Date.now() };
  localStorage.setItem(SESSION_KEY, JSON.stringify(stored));
}

export function loadSession(): LoginResponse | null {
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

export function clearSession(): void {
  localStorage.removeItem(SESSION_KEY);
}

/** Parse JWT exp claim without verification */
export function parseJwtExp(token: string): number | null {
  try {
    const payload = token.split(".")[1];
    const decoded = JSON.parse(atob(payload)) as { exp?: number };
    return decoded.exp ?? null;
  } catch {
    return null;
  }
}

/** Send release beacon to start server-side grace period cleanup.
 *  Uses navigator.sendBeacon() which reliably fires during tab close. */
export function sendReleaseBeacon(sessionId: string | null, releaseToken: string | null): void {
  if (sessionId && releaseToken) {
    navigator.sendBeacon(
      `/api/sessions/${sessionId}/release`,
      releaseToken,
    );
  }
}

/**
 * Token refresh manager. Handles scheduling proactive JWT refresh
 * and executing the refresh API call.
 */
export class TokenManager {
  private currentToken: string | null = null;
  private refreshTimer: ReturnType<typeof setTimeout> | null = null;
  private connection: BeamConnection | null = null;

  getToken(): string | null {
    return this.currentToken;
  }

  setToken(token: string): void {
    this.currentToken = token;
    const data = loadSession();
    if (data) {
      data.token = token;
      saveSession(data);
    }
  }

  setConnection(conn: BeamConnection | null): void {
    this.connection = conn;
  }

  clearToken(): void {
    this.currentToken = null;
    if (this.refreshTimer) {
      clearTimeout(this.refreshTimer);
      this.refreshTimer = null;
    }
  }

  /** Schedule proactive token refresh 5 minutes before expiry */
  scheduleTokenRefresh(): void {
    if (this.refreshTimer) {
      clearTimeout(this.refreshTimer);
      this.refreshTimer = null;
    }
    if (!this.currentToken) return;

    const exp = parseJwtExp(this.currentToken);
    if (!exp) return;

    const nowSec = Math.floor(Date.now() / 1000);
    const refreshInMs = (exp - nowSec - 300) * 1000; // 5 min before expiry

    if (refreshInMs <= 0) {
      this.refreshToken();
      return;
    }

    this.refreshTimer = setTimeout(() => {
      this.refreshTimer = null;
      this.refreshToken();
    }, refreshInMs);
  }

  /** Attempt to refresh the JWT token */
  async refreshToken(): Promise<boolean> {
    if (!this.currentToken) return false;
    try {
      const resp = await fetch("/api/auth/refresh", {
        method: "POST",
        headers: { Authorization: `Bearer ${this.currentToken}` },
      });
      if (!resp.ok) return false;
      const data = (await resp.json()) as { token: string };
      this.setToken(data.token);
      this.connection?.updateToken(data.token);
      this.scheduleTokenRefresh();
      console.log("Token refreshed");
      return true;
    } catch {
      console.warn("Token refresh failed");
      return false;
    }
  }
}
