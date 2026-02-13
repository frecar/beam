use std::sync::Arc;

use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use beam_protocol::{AuthRequest, AuthResponse, BeamConfig, IceServerInfo, SignalingMessage};
use serde::Deserialize;
use serde_json::json;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::services::ServeDir;
use uuid::Uuid;

use crate::auth;
use crate::session::SessionManager;
use crate::signaling::{self, ChannelRegistry};

/// Shared application state.
pub struct AppState {
    pub config: BeamConfig,
    pub session_manager: SessionManager,
    pub channels: ChannelRegistry,
    pub jwt_secret: String,
    pub login_limiter: LoginRateLimiter,
    pub started_at: std::time::Instant,
}

/// Simple per-key rate limiter for login attempts.
/// Allows at most `max_attempts` in `window_secs`.
/// Bounded to prevent memory exhaustion from enumeration attacks.
/// Performs automatic TTL cleanup every `ttl_cleanup_interval` calls to `check()`.
pub struct LoginRateLimiter {
    attempts: std::sync::Mutex<std::collections::HashMap<String, Vec<std::time::Instant>>>,
    max_attempts: usize,
    window: std::time::Duration,
    /// Maximum number of unique keys to track (prevents unbounded growth)
    max_keys: usize,
    /// Counter for periodic TTL cleanup (every Nth call to check())
    call_count: std::sync::atomic::AtomicU64,
    /// Run TTL cleanup every this many calls to check()
    ttl_cleanup_interval: u64,
}

impl LoginRateLimiter {
    pub fn new(max_attempts: usize, window_secs: u64) -> Self {
        Self {
            attempts: std::sync::Mutex::new(std::collections::HashMap::new()),
            max_attempts,
            window: std::time::Duration::from_secs(window_secs),
            max_keys: 10_000,
            call_count: std::sync::atomic::AtomicU64::new(0),
            ttl_cleanup_interval: 100,
        }
    }

    /// Check if a login attempt from this key (IP or username) is allowed.
    /// Returns true if allowed, false if rate-limited.
    pub fn check(&self, key: &str) -> bool {
        let mut attempts = self.attempts.lock().unwrap_or_else(|e| e.into_inner());
        let now = std::time::Instant::now();

        // Periodic TTL cleanup: prune all expired entries every N calls.
        // This prevents unbounded memory growth from enumeration attacks
        // where many unique keys are used but never repeated.
        let count = self
            .call_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if count.is_multiple_of(self.ttl_cleanup_interval) || attempts.len() > self.max_keys / 2 {
            attempts.retain(|_k, timestamps| {
                timestamps.retain(|t| now.duration_since(*t) < self.window);
                !timestamps.is_empty()
            });
        }

        // Hard cap: if still too many keys, reject (defensive against DoS)
        if attempts.len() >= self.max_keys && !attempts.contains_key(key) {
            return false;
        }

        let entry = attempts.entry(key.to_string()).or_default();

        // Remove expired attempts for this key
        entry.retain(|t| now.duration_since(*t) < self.window);

        if entry.len() >= self.max_attempts {
            return false;
        }

        entry.push(now);
        true
    }

    /// Clear rate limit entries for a key (e.g., after successful login).
    pub fn clear(&self, key: &str) {
        let mut attempts = self.attempts.lock().unwrap_or_else(|e| e.into_inner());
        attempts.remove(key);
    }

    /// Return the number of unique keys currently tracked.
    #[cfg(test)]
    fn key_count(&self) -> usize {
        let attempts = self.attempts.lock().unwrap_or_else(|e| e.into_inner());
        attempts.len()
    }

    /// Create a limiter with a custom TTL cleanup interval (for testing).
    #[cfg(test)]
    fn with_cleanup_interval(mut self, interval: u64) -> Self {
        self.ttl_cleanup_interval = interval;
        self
    }
}

/// Build the Axum router with all routes.
pub fn build_router(state: Arc<AppState>) -> Router {
    let api = Router::new()
        .route("/api/auth/login", post(login))
        .route("/api/auth/refresh", post(refresh_token))
        .route("/api/sessions", get(list_sessions))
        .route("/api/sessions/{id}", delete(delete_session))
        .route("/api/sessions/{id}/release", post(release_session))
        .route("/api/sessions/{id}/heartbeat", post(session_heartbeat))
        .route("/api/sessions/{id}/ws", get(browser_ws_upgrade))
        .route("/api/health", get(health_check))
        .route("/api/health/detailed", get(health_check_detailed))
        .route("/api/ice-config", get(ice_config))
        .route("/ws/agent/{id}", get(agent_ws_upgrade))
        .layer(RequestBodyLimitLayer::new(65_536)) // 64KB max request body
        .with_state(Arc::clone(&state));

    // Serve static files (configurable path, defaults to "web/dist")
    let serve_dir = ServeDir::new(&state.config.server.web_root);

    api.fallback_service(serve_dir)
}

/// Query parameters for WebSocket upgrade
#[derive(Deserialize)]
struct WsQuery {
    token: Option<String>,
}

/// Extract and validate JWT from Authorization header or query parameter.
/// Prefers the Authorization header (Bearer token) when available.
fn extract_claims_from_headers(
    headers: &HeaderMap,
    query: &WsQuery,
    jwt_secret: &str,
) -> Result<auth::Claims, (StatusCode, String)> {
    // Try Authorization: Bearer <token> header first
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        // Fall back to query parameter
        .or(query.token.as_deref())
        .ok_or_else(|| (StatusCode::UNAUTHORIZED, "Missing token".to_string()))?;

    auth::validate_jwt(token, jwt_secret).map_err(|e| {
        tracing::warn!("Invalid JWT: {e}");
        (
            StatusCode::UNAUTHORIZED,
            "Invalid or expired token".to_string(),
        )
    })
}

/// Validate that a username is non-empty, at most 64 chars, and contains only
/// alphanumeric ASCII characters plus `_`, `-`, and `.`.
fn is_valid_username(username: &str) -> bool {
    !username.is_empty()
        && username.len() <= 64
        && username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

/// POST /api/auth/login
///
/// Authenticate via PAM and return a JWT + session.
async fn login(
    State(state): State<Arc<AppState>>,
    Json(req): Json<AuthRequest>,
) -> impl IntoResponse {
    tracing::info!(username = %req.username, "Login request");

    // Validate username before anything else (before rate limiter to avoid
    // polluting the limiter with garbage keys from fuzzing/scanning).
    if !is_valid_username(&req.username) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "Invalid username" })),
        )
            .into_response();
    }

    // Rate limit: check BEFORE auth to avoid wasting PAM calls, but only
    // count failed attempts (so attackers can't lock out legitimate users
    // by sending bad requests with their username).
    if !state.login_limiter.check(&req.username) {
        tracing::warn!(username = %req.username, "Login rate limited");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "error": "Too many login attempts. Please try again later." })),
        )
            .into_response();
    }

    // Run PAM authentication in a blocking task with timeout to avoid hanging
    // on misconfigured LDAP/SSSD backends
    let username = req.username.clone();
    let password = req.password.clone();
    let pam_result = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        tokio::task::spawn_blocking(move || auth::authenticate_pam(&username, &password)),
    )
    .await;

    match pam_result {
        Err(_) => {
            tracing::warn!(username = %req.username, "PAM authentication timed out (10s)");
            return (
                StatusCode::GATEWAY_TIMEOUT,
                Json(json!({ "error": "Authentication timed out" })),
            )
                .into_response();
        }
        Ok(Ok(Ok(()))) => {
            // Successful auth — clear rate limit so legitimate users aren't affected
            // by earlier failed attempts (e.g., typos or attacker lockout attempts)
            state.login_limiter.clear(&req.username);
        }
        Ok(Ok(Err(e))) => {
            tracing::warn!(username = %req.username, "Authentication failed: {e}");
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Invalid credentials" })),
            )
                .into_response();
        }
        Ok(Err(e)) => {
            tracing::error!("PAM task panicked: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Internal server error" })),
            )
                .into_response();
        }
    }

    // Generate JWT
    let token = match auth::generate_jwt(&req.username, &state.jwt_secret) {
        Ok(t) => t,
        Err(e) => {
            tracing::error!("Failed to generate JWT: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Internal server error" })),
            )
                .into_response();
        }
    };

    // Reuse existing session if the user already has one running.
    // The agent handles new WebRTC connections (SDP renegotiation), so the
    // desktop state (windows, files, etc.) is preserved across reconnects.
    if let Some(existing) = state.session_manager.find_by_username(&req.username).await {
        tracing::info!(
            session_id = %existing.id,
            username = %req.username,
            "Reusing existing session"
        );
        // Ensure signaling channel exists (may have been cleaned up)
        signaling::get_or_create_channel(&state.channels, existing.id).await;

        // Cancel any pending grace-period cleanup since the user is reconnecting
        state.session_manager.cancel_grace_period(existing.id).await;

        let release_token = state.session_manager.get_release_token(existing.id).await;

        return (
            StatusCode::OK,
            Json(json!(AuthResponse {
                token,
                session_id: existing.id,
                release_token,
            })),
        )
            .into_response();
    }

    // No existing session — create a new one
    let server_url = format!("wss://127.0.0.1:{}", state.config.server.port);
    let max_sessions = state.config.session.max_sessions as usize;

    let session = match state
        .session_manager
        .create_session(
            &req.username,
            &server_url,
            max_sessions,
            req.viewport_width,
            req.viewport_height,
        )
        .await
    {
        Ok(s) => s,
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("Maximum number of sessions") {
                tracing::warn!(username = %req.username, "Max sessions reached");
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    Json(json!({ "error": msg })),
                )
                    .into_response();
            }
            tracing::error!("Failed to create session: {e}");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Failed to create session" })),
            )
                .into_response();
        }
    };

    // Create signaling channel
    signaling::get_or_create_channel(&state.channels, session.id).await;

    // Monitor agent process in the background
    spawn_agent_monitor(Arc::clone(&state), session.id).await;

    let release_token = state.session_manager.get_release_token(session.id).await;

    tracing::info!(
        session_id = %session.id,
        username = %req.username,
        display = session.display,
        "Session created"
    );

    (
        StatusCode::OK,
        Json(json!(AuthResponse {
            token,
            session_id: session.id,
            release_token,
        })),
    )
        .into_response()
}

/// POST /api/auth/refresh
///
/// Accept a valid or recently-expired JWT and return a fresh one.
/// Does NOT require re-authentication via PAM.
async fn refresh_token(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<WsQuery>,
) -> impl IntoResponse {
    let token = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .or(query.token.as_deref());

    let token = match token {
        Some(t) => t,
        None => {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Missing token" })),
            )
                .into_response();
        }
    };

    let claims = match auth::validate_jwt_for_refresh(token, &state.jwt_secret) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("Token refresh rejected: {e}");
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "error": "Token cannot be refreshed" })),
            )
                .into_response();
        }
    };

    // Only refresh if user still has an active session
    if state
        .session_manager
        .find_by_username(&claims.sub)
        .await
        .is_none()
    {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "No active session" })),
        )
            .into_response();
    }

    match auth::generate_jwt(&claims.sub, &state.jwt_secret) {
        Ok(new_token) => {
            tracing::info!(username = %claims.sub, "Token refreshed");
            Json(json!({ "token": new_token })).into_response()
        }
        Err(e) => {
            tracing::error!("Failed to generate refreshed JWT: {e}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": "Internal server error" })),
            )
                .into_response()
        }
    }
}

/// Spawn a background task that monitors the agent process for unexpected exit.
async fn spawn_agent_monitor(state: Arc<AppState>, session_id: Uuid) {
    if let Some(mut child) = state.session_manager.take_agent_child(session_id).await {
        tokio::spawn(async move {
            let status = child.wait().await;
            match status {
                Ok(exit_status) if exit_status.success() => {
                    tracing::info!(%session_id, "Agent exited cleanly");
                }
                Ok(exit_status) => {
                    tracing::error!(%session_id, ?exit_status, "Agent exited unexpectedly");
                    // Agent stderr/stdout goes to /tmp/beam-agent-{session_id}.log
                }
                Err(e) => {
                    tracing::error!(%session_id, "Failed to wait for agent: {e}");
                }
            }

            // Notify the browser before destroying the session so it
            // gets a clear error instead of 30s of failed reconnect attempts.
            {
                let channels = state.channels.read().await;
                if let Some(channel) = channels.get(&session_id) {
                    let _ = channel.to_browser.send(SignalingMessage::Error {
                        message: "agent_exited".to_string(),
                    });
                }
            }

            // Clean up the session
            if let Err(e) = state.session_manager.destroy_session(session_id).await {
                tracing::error!(%session_id, "Failed to clean up after agent exit: {e}");
            }
            signaling::remove_channel(&state.channels, session_id).await;
            tracing::info!(%session_id, "Session cleaned up after agent exit");
        });
    }
}

/// Monitor a restored agent by polling kill(pid, 0).
/// Unlike spawn_agent_monitor, this works for orphaned processes
/// where we don't have a Child handle.
pub async fn spawn_orphan_agent_monitor(state: Arc<AppState>, session_id: Uuid, pid: u32) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            // Check process is alive AND is actually beam-agent (guards against PID recycling)
            let is_agent = std::fs::read_to_string(format!("/proc/{pid}/comm"))
                .map(|name| name.trim() == "beam-agent")
                .unwrap_or(false);
            if !is_agent {
                tracing::warn!(%session_id, pid, "Restored agent process exited (or PID recycled)");
                break;
            }
        }

        // Notify browser
        {
            let channels = state.channels.read().await;
            if let Some(channel) = channels.get(&session_id) {
                let _ = channel.to_browser.send(SignalingMessage::Error {
                    message: "agent_exited".to_string(),
                });
            }
        }

        // Clean up
        if let Err(e) = state.session_manager.destroy_session(session_id).await {
            tracing::error!(%session_id, "Failed to clean up restored agent: {e}");
        }
        signaling::remove_channel(&state.channels, session_id).await;
        tracing::info!(%session_id, "Restored session cleaned up after agent exit");
    });
}

/// GET /api/sessions - requires JWT auth (returns only the caller's sessions)
async fn list_sessions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<WsQuery>,
) -> impl IntoResponse {
    let claims = match extract_claims_from_headers(&headers, &query, &state.jwt_secret) {
        Ok(c) => c,
        Err((status, msg)) => {
            return (status, Json(json!({ "error": msg }))).into_response();
        }
    };

    // Only return sessions belonging to the authenticated user
    let list: Vec<_> = state
        .session_manager
        .list_sessions()
        .await
        .into_iter()
        .filter(|s| s.username == claims.sub)
        .collect();
    Json(list).into_response()
}

/// GET /api/sessions/:id/ws - WebSocket upgrade for browser signaling (requires JWT + session ownership)
async fn browser_ws_upgrade(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Query(query): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let claims = match extract_claims_from_headers(&headers, &query, &state.jwt_secret) {
        Ok(c) => c,
        Err((status, msg)) => return (status, msg).into_response(),
    };

    // Verify session exists and belongs to the authenticated user
    match state.session_manager.get_session(id).await {
        Some(session) if session.username == claims.sub => {}
        Some(_) => {
            tracing::warn!(%id, user = %claims.sub, "Session ownership mismatch");
            return (StatusCode::FORBIDDEN, "Access denied").into_response();
        }
        None => {
            return (StatusCode::NOT_FOUND, "Session not found").into_response();
        }
    }

    // Cancel any pending grace-period cleanup since a browser is reconnecting
    state.session_manager.cancel_grace_period(id).await;

    tracing::info!(%id, "Browser WebSocket upgrade");
    let channels = state.channels.clone();
    ws.max_message_size(65_536) // 64KB max for signaling messages
        .on_upgrade(move |socket| signaling::handle_browser_ws(socket, id, channels))
        .into_response()
}

/// POST /api/sessions/:id/heartbeat - update session activity (requires JWT + session ownership)
async fn session_heartbeat(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Query(query): Query<WsQuery>,
) -> impl IntoResponse {
    let claims = match extract_claims_from_headers(&headers, &query, &state.jwt_secret) {
        Ok(c) => c,
        Err((status, msg)) => return (status, msg).into_response(),
    };

    // Verify session ownership
    match state.session_manager.get_session(id).await {
        Some(session) if session.username == claims.sub => {}
        Some(_) => {
            return (StatusCode::FORBIDDEN, "Access denied").into_response();
        }
        None => {
            return (StatusCode::NOT_FOUND, "Session not found").into_response();
        }
    }

    state.session_manager.heartbeat(id).await;
    (StatusCode::OK, "OK").into_response()
}

/// DELETE /api/sessions/:id - destroy a session (requires JWT + session ownership)
async fn delete_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    headers: HeaderMap,
    Query(query): Query<WsQuery>,
) -> impl IntoResponse {
    let claims = match extract_claims_from_headers(&headers, &query, &state.jwt_secret) {
        Ok(c) => c,
        Err((status, msg)) => return (status, msg).into_response(),
    };

    // Verify session ownership
    match state.session_manager.get_session(id).await {
        Some(session) if session.username == claims.sub => {}
        Some(_) => {
            tracing::warn!(%id, user = %claims.sub, "Unauthorized session delete attempt");
            return (StatusCode::FORBIDDEN, "Access denied").into_response();
        }
        None => {
            return (StatusCode::NOT_FOUND, "Session not found").into_response();
        }
    }

    // Destroy session (kills agent, recycles display)
    if let Err(e) = state.session_manager.destroy_session(id).await {
        tracing::error!(%id, "Failed to destroy session: {e}");
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            "Failed to destroy session",
        )
            .into_response();
    }

    // Clean up signaling channel
    signaling::remove_channel(&state.channels, id).await;

    (StatusCode::OK, "Session destroyed").into_response()
}

/// POST /api/sessions/:id/release - graceful session release on browser tab close.
///
/// Called via `navigator.sendBeacon()` which cannot set Authorization headers,
/// so this endpoint uses a separate release token in the request body instead
/// of JWT auth. The release token is returned alongside the JWT at login time.
///
/// Starts a 60-second grace period. If no browser WebSocket reconnects within
/// that window, the session is destroyed. This handles the common case of
/// closing a tab without clicking "End Session".
async fn release_session(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    body: String,
) -> impl IntoResponse {
    // sendBeacon sends as text/plain — the body IS the release token.
    // Trim whitespace/newlines that browsers or proxies might add.
    let token = body.trim();

    if token.is_empty() {
        return (StatusCode::BAD_REQUEST, "Missing release token").into_response();
    }

    if !state.session_manager.verify_release_token(id, token).await {
        // Don't reveal whether the session exists or the token is wrong
        return (StatusCode::UNAUTHORIZED, "Invalid release token").into_response();
    }

    tracing::info!(%id, "Session release requested, starting 60s grace period");

    // Get the cancel flag and spawn the grace-period cleanup task
    let cancel = match state.session_manager.start_grace_period(id).await {
        Some(c) => c,
        None => {
            return (StatusCode::NOT_FOUND, "Session not found").into_response();
        }
    };

    let state_clone = Arc::clone(&state);
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;

        // Check if the grace period was cancelled (browser reconnected)
        if cancel.load(std::sync::atomic::Ordering::SeqCst) {
            tracing::info!(%id, "Grace period cancelled — browser reconnected");
            return;
        }

        tracing::info!(%id, "Grace period expired — destroying session");
        if let Err(e) = state_clone.session_manager.destroy_session(id).await {
            tracing::error!(%id, "Failed to destroy session after grace period: {e}");
        }
        signaling::remove_channel(&state_clone.channels, id).await;
    });

    (StatusCode::OK, "Release accepted").into_response()
}

/// GET /api/health - server health check (no auth required, minimal info for load balancers)
async fn health_check() -> impl IntoResponse {
    Json(json!({ "status": "ok" }))
}

/// GET /api/health/detailed - full health info (requires JWT auth)
async fn health_check_detailed(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<WsQuery>,
) -> impl IntoResponse {
    let claims = match extract_claims_from_headers(&headers, &query, &state.jwt_secret) {
        Ok(c) => c,
        Err((status, msg)) => {
            return (status, Json(json!({ "error": msg }))).into_response();
        }
    };

    let _ = claims; // authenticated — no further authorization needed

    let sessions = state.session_manager.list_sessions().await;
    Json(json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "uptime_secs": state.started_at.elapsed().as_secs(),
        "sessions": sessions.len(),
    }))
    .into_response()
}

/// GET /api/ice-config - return ICE/TURN server configuration (requires JWT)
async fn ice_config(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Query(query): Query<WsQuery>,
) -> impl IntoResponse {
    if let Err((status, msg)) = extract_claims_from_headers(&headers, &query, &state.jwt_secret) {
        return (status, Json(json!({ "error": msg }))).into_response();
    }

    let ice = &state.config.ice;
    let mut servers = Vec::new();

    // Add STUN servers
    if !ice.stun_urls.is_empty() {
        servers.push(IceServerInfo {
            urls: ice.stun_urls.clone(),
            username: None,
            credential: None,
        });
    }

    // Add TURN servers
    if !ice.turn_urls.is_empty() {
        servers.push(IceServerInfo {
            urls: ice.turn_urls.clone(),
            username: ice.turn_username.clone(),
            credential: ice.turn_credential.clone(),
        });
    }

    Json(json!({ "ice_servers": servers })).into_response()
}

/// GET /ws/agent/:id - WebSocket upgrade for agent signaling (requires agent token)
async fn agent_ws_upgrade(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
    Query(query): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    // Validate agent token
    let token = match &query.token {
        Some(t) => t,
        None => {
            return (StatusCode::UNAUTHORIZED, "Missing agent token").into_response();
        }
    };

    if !state.session_manager.verify_agent_token(id, token).await {
        tracing::warn!(%id, "Invalid agent token on WebSocket upgrade");
        return (StatusCode::UNAUTHORIZED, "Invalid agent token").into_response();
    }

    tracing::info!(%id, "Agent WebSocket upgrade (authenticated)");
    let channels = state.channels.clone();
    ws.max_message_size(65_536) // 64KB max for signaling messages
        .on_upgrade(move |socket| signaling::handle_agent_ws(socket, id, channels))
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_limiter_allows_under_limit() {
        let limiter = LoginRateLimiter::new(3, 60);
        assert!(limiter.check("user1"));
        assert!(limiter.check("user1"));
        assert!(limiter.check("user1"));
    }

    #[test]
    fn rate_limiter_blocks_over_limit() {
        let limiter = LoginRateLimiter::new(3, 60);
        assert!(limiter.check("user1"));
        assert!(limiter.check("user1"));
        assert!(limiter.check("user1"));
        // 4th attempt should be blocked
        assert!(!limiter.check("user1"));
    }

    #[test]
    fn rate_limiter_independent_per_key() {
        let limiter = LoginRateLimiter::new(2, 60);
        assert!(limiter.check("user1"));
        assert!(limiter.check("user1"));
        assert!(!limiter.check("user1")); // blocked

        // user2 should still be allowed
        assert!(limiter.check("user2"));
        assert!(limiter.check("user2"));
    }

    #[test]
    fn rate_limiter_resets_after_window() {
        let limiter = LoginRateLimiter::new(2, 0); // 0-second window = immediately expires
        assert!(limiter.check("user1"));
        assert!(limiter.check("user1"));
        // With a 0-second window, previous attempts expire immediately
        std::thread::sleep(std::time::Duration::from_millis(10));
        assert!(limiter.check("user1"));
    }

    #[test]
    fn rate_limiter_ttl_cleanup_removes_expired_entries() {
        // window=0s means entries expire immediately; cleanup_interval=1
        // means every call to check() triggers a full TTL sweep.
        let limiter = LoginRateLimiter::new(5, 0).with_cleanup_interval(1);

        // Simulate an enumeration attack: 50 unique keys
        for i in 0..50 {
            limiter.check(&format!("attacker-{i}"));
        }

        // Wait for all entries to expire (window=0s, so any delay suffices)
        std::thread::sleep(std::time::Duration::from_millis(10));

        // Next check() triggers TTL cleanup, pruning all 50 expired keys
        limiter.check("trigger-cleanup");

        // Only the freshly-inserted key should remain
        assert_eq!(
            limiter.key_count(),
            1,
            "Expired entries should be pruned by TTL cleanup"
        );
    }

    #[test]
    fn rate_limiter_ttl_cleanup_preserves_active_entries() {
        // Use a 60-second window with cleanup on every call
        let limiter = LoginRateLimiter::new(5, 60).with_cleanup_interval(1);

        limiter.check("active-user-1");
        limiter.check("active-user-2");
        limiter.check("active-user-3");

        // Trigger another cleanup — all entries are within window, none should be pruned
        limiter.check("active-user-4");

        assert_eq!(
            limiter.key_count(),
            4,
            "Active entries should not be pruned"
        );
    }

    #[test]
    fn extract_claims_from_bearer_header() {
        let secret = "test-secret";
        let token = crate::auth::generate_jwt("alice", secret).unwrap();

        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
        let query = WsQuery { token: None };

        let claims = extract_claims_from_headers(&headers, &query, secret).unwrap();
        assert_eq!(claims.sub, "alice");
    }

    #[test]
    fn extract_claims_from_query_fallback() {
        let secret = "test-secret";
        let token = crate::auth::generate_jwt("bob", secret).unwrap();

        let headers = HeaderMap::new();
        let query = WsQuery { token: Some(token) };

        let claims = extract_claims_from_headers(&headers, &query, secret).unwrap();
        assert_eq!(claims.sub, "bob");
    }

    #[test]
    fn extract_claims_prefers_header_over_query() {
        let secret = "test-secret";
        let header_token = crate::auth::generate_jwt("alice", secret).unwrap();
        let query_token = crate::auth::generate_jwt("bob", secret).unwrap();

        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            format!("Bearer {header_token}").parse().unwrap(),
        );
        let query = WsQuery {
            token: Some(query_token),
        };

        // Header should take precedence
        let claims = extract_claims_from_headers(&headers, &query, secret).unwrap();
        assert_eq!(claims.sub, "alice");
    }

    #[test]
    fn extract_claims_rejects_missing_token() {
        let headers = HeaderMap::new();
        let query = WsQuery { token: None };
        let result = extract_claims_from_headers(&headers, &query, "secret");
        assert!(result.is_err());
    }

    #[test]
    fn extract_claims_rejects_invalid_token() {
        let headers = HeaderMap::new();
        let query = WsQuery {
            token: Some("invalid.token.here".to_string()),
        };
        let result = extract_claims_from_headers(&headers, &query, "secret");
        assert!(result.is_err());
    }

    #[test]
    fn username_validation_rejects_empty() {
        assert!(!is_valid_username(""));
    }

    #[test]
    fn username_validation_rejects_too_long() {
        let long = "a".repeat(65);
        assert!(!is_valid_username(&long));
    }

    #[test]
    fn username_validation_rejects_invalid_chars() {
        assert!(!is_valid_username("user name")); // space
        assert!(!is_valid_username("user@host")); // @
        assert!(!is_valid_username("user/root")); // path traversal
        assert!(!is_valid_username("user\x00")); // null byte
        assert!(!is_valid_username("user;id")); // shell injection
    }

    #[test]
    fn username_validation_accepts_valid() {
        assert!(is_valid_username("alice"));
        assert!(is_valid_username("bob_smith"));
        assert!(is_valid_username("user-123"));
        assert!(is_valid_username("test.user"));
        assert!(is_valid_username("A"));
        assert!(is_valid_username(&"a".repeat(64))); // exactly 64 chars
    }

    // --- HTTP-level integration tests ---
    //
    // These use `tower::ServiceExt::oneshot` to send requests through the axum
    // router without starting a real HTTP server or TLS listener.

    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const TEST_JWT_SECRET: &str = "test-secret-for-integration-tests";

    /// Build a test `AppState` with defaults suitable for unit/integration tests.
    fn test_app_state() -> Arc<AppState> {
        let config: BeamConfig = toml::from_str("").expect("default config");
        let session_manager = crate::session::SessionManager::new(
            100, // display_start (high to avoid conflicts)
            1920,
            1080,
            None,
            None,
            beam_protocol::VideoConfig::default(),
        );
        Arc::new(AppState {
            config,
            session_manager,
            channels: crate::signaling::new_channel_registry(),
            jwt_secret: TEST_JWT_SECRET.to_string(),
            login_limiter: LoginRateLimiter::new(5, 60),
            started_at: std::time::Instant::now(),
        })
    }

    /// Helper: parse a response body as `serde_json::Value`.
    async fn body_json(response: axum::response::Response<Body>) -> serde_json::Value {
        let bytes = response
            .into_body()
            .collect()
            .await
            .expect("failed to read response body")
            .to_bytes();
        serde_json::from_slice(&bytes).expect("response body is not valid JSON")
    }

    #[tokio::test]
    async fn health_returns_ok_unauthenticated() {
        let state = test_app_state();
        let app = build_router(state);

        let request = Request::builder()
            .uri("/api/health")
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let json = body_json(response).await;
        assert_eq!(json["status"], "ok");
    }

    #[tokio::test]
    async fn health_detailed_requires_auth() {
        let state = test_app_state();
        let app = build_router(state);

        let request = Request::builder()
            .uri("/api/health/detailed")
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn health_detailed_with_valid_jwt() {
        let state = test_app_state();
        let app = build_router(state);

        let token = crate::auth::generate_jwt("testuser", TEST_JWT_SECRET).unwrap();

        let request = Request::builder()
            .uri("/api/health/detailed")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let json = body_json(response).await;
        assert_eq!(json["status"], "ok");
        assert!(json["version"].is_string(), "expected version string");
        assert!(json["uptime_secs"].is_number(), "expected uptime number");
        assert!(json["sessions"].is_number(), "expected sessions count");
    }

    #[tokio::test]
    async fn list_sessions_requires_auth() {
        let state = test_app_state();
        let app = build_router(state);

        let request = Request::builder()
            .uri("/api/sessions")
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn list_sessions_with_valid_jwt() {
        let state = test_app_state();
        let app = build_router(state);

        let token = crate::auth::generate_jwt("testuser", TEST_JWT_SECRET).unwrap();

        let request = Request::builder()
            .uri("/api/sessions")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let json = body_json(response).await;
        assert!(json.is_array(), "expected JSON array of sessions");
        // No sessions created, so the array should be empty
        assert_eq!(json.as_array().unwrap().len(), 0);
    }

    #[tokio::test]
    async fn login_returns_401_for_invalid_creds() {
        let state = test_app_state();
        let app = build_router(state);

        let body = serde_json::json!({
            "username": "nonexistent",
            "password": "wrongpassword"
        });

        let request = Request::builder()
            .method("POST")
            .uri("/api/auth/login")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body).unwrap()))
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        // PAM auth will fail (no such user) — expect 401 or 504 (timeout)
        // depending on PAM backend speed. Either way, NOT 200.
        let status = response.status();
        assert!(
            status == StatusCode::UNAUTHORIZED
                || status == StatusCode::GATEWAY_TIMEOUT
                || status == StatusCode::INTERNAL_SERVER_ERROR,
            "expected auth failure status, got {status}"
        );

        let json = body_json(response).await;
        assert!(json["error"].is_string(), "expected error message in body");
    }

    #[tokio::test]
    async fn invalid_jwt_rejected() {
        let state = test_app_state();
        let app = build_router(state);

        // Generate a JWT signed with a different secret
        let wrong_token =
            crate::auth::generate_jwt("testuser", "completely-different-secret").unwrap();

        let request = Request::builder()
            .uri("/api/sessions")
            .header("authorization", format!("Bearer {wrong_token}"))
            .body(Body::empty())
            .unwrap();

        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn username_validation_rejects_bad_input() {
        let bad_usernames = vec![
            "../etc/passwd", // path traversal
            "user\x00admin", // null byte
            "user name",     // space
            "user;id",       // shell injection
            "",              // empty
        ];

        for bad_username in bad_usernames {
            let state = test_app_state();
            let app = build_router(state);

            let body = serde_json::json!({
                "username": bad_username,
                "password": "anything"
            });

            let request = Request::builder()
                .method("POST")
                .uri("/api/auth/login")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap();

            let response = app.oneshot(request).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "username {bad_username:?} should be rejected with 400"
            );

            let json = body_json(response).await;
            assert_eq!(json["error"], "Invalid username");
        }
    }
}
