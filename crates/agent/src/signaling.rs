use crate::CaptureCommand;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use beam_protocol::{AgentCommand, InputEvent};
use tokio::sync::mpsc;
use tracing::{info, warn};
use uuid::Uuid;

/// Type alias for the shared WebSocket sender.
/// Both signaling (text JSON) and video/audio (binary frames) use this.
pub(crate) type WsSender = mpsc::Sender<tokio_tungstenite::tungstenite::Message>;

/// Shared context for signaling WebSocket connection.
pub(crate) struct SignalingCtx<'a> {
    pub server_url: &'a str,
    pub session_id: Uuid,
    pub agent_token: Option<&'a str>,
    pub tls_cert_path: Option<&'a str>,
    pub force_keyframe: Arc<AtomicBool>,
    pub input_callback: Arc<dyn Fn(InputEvent) + Send + Sync>,
    pub capture_cmd_tx: &'a std::sync::mpsc::Sender<CaptureCommand>,
    pub tab_backgrounded: Arc<AtomicBool>,
}

/// Run the signaling WebSocket connection with reconnect.
///
/// `ws_outbox_rx` receives outgoing WS messages from video/audio/clipboard/cursor tasks.
/// Incoming WS text messages (AgentCommand) are dispatched to the input callback.
pub(crate) async fn run_signaling(
    ctx: &SignalingCtx<'_>,
    ws_outbox_rx: &mut mpsc::Receiver<tokio_tungstenite::tungstenite::Message>,
) {
    if ctx.server_url.is_empty() {
        info!("{}", no_server_url_message());
        std::future::pending::<()>().await;
        return;
    }

    // Connect to WebSocket with exponential backoff retry
    let mut backoff = INITIAL_BACKOFF;
    loop {
        info!("{}", connecting_message(ctx.server_url));

        match connect_and_handle(ctx, ws_outbox_rx).await {
            Ok(()) => {
                info!("{}", closed_cleanly_message());
                break;
            }
            Err(e) => {
                warn!("{}", connection_error_message(&format!("{e:#}")));
                info!("{}", reconnecting_message(backoff.as_secs()));
                tokio::time::sleep(backoff).await;
                backoff = next_backoff(backoff, MAX_BACKOFF);
            }
        }
    }
}

/// Build a TLS connector, pinning the server certificate if a cert path is provided.
fn build_tls_connector(tls_cert_path: Option<&str>) -> tokio_tungstenite::Connector {
    let mut root_store = rustls::RootCertStore::empty();

    for cert in rustls_native_certs::load_native_certs().expect("Could not load platform certs") {
        let _ = root_store.add(cert);
    }

    if let Some(cert_path) = tls_cert_path {
        match std::fs::read(cert_path) {
            Ok(pem_data) => {
                let certs: Vec<_> = rustls_pemfile::certs(&mut pem_data.as_slice())
                    .filter_map(|r| r.ok())
                    .collect();
                for cert in certs {
                    if let Err(e) = root_store.add(cert) {
                        warn!("Failed to add pinned cert to root store: {e}");
                    } else {
                        info!("Pinned server certificate from {cert_path}");
                    }
                }
            }
            Err(e) => {
                warn!(
                    "Failed to read TLS cert from {cert_path}: {e}, falling back to system roots"
                );
            }
        }
    }

    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    tokio_tungstenite::Connector::Rustls(Arc::new(tls_config))
}

/// Build the websocket URL for connecting an agent to a beam-server.
///
/// Split out so the URL formatting + token-encoding logic can be exercised
/// without a live network connection. URL-encodes the agent token so tokens
/// containing reserved URL characters don't break the query string.
pub(crate) fn build_agent_ws_url(
    server_url: &str,
    session_id: uuid::Uuid,
    agent_token: Option<&str>,
) -> String {
    match agent_token {
        Some(token) => format!(
            "{}/ws/agent/{}?token={}",
            server_url,
            session_id,
            urlencoding::encode(token)
        ),
        None => format!("{}/ws/agent/{}", server_url, session_id),
    }
}

/// Outcome of parsing a server→agent text frame.
#[derive(Debug)]
pub(crate) enum ServerTextOutcome {
    /// An input event to forward to the local input callback.
    Input(InputEvent),
    /// A shutdown command — clean exit.
    Shutdown,
    /// Unparseable / unknown JSON — log + ignore.
    Invalid(String),
}

/// Parse a server-to-agent text frame as [`AgentCommand`]. Split out so the
/// dispatch + error handling can be unit-tested without a live WebSocket.
pub(crate) fn parse_server_text(text: &str) -> ServerTextOutcome {
    match serde_json::from_str::<AgentCommand>(text) {
        Ok(AgentCommand::Input(event)) => ServerTextOutcome::Input(event),
        Ok(AgentCommand::Shutdown) => ServerTextOutcome::Shutdown,
        Err(e) => ServerTextOutcome::Invalid(e.to_string()),
    }
}

/// Exponential backoff step used by the signaling reconnect loop. The
/// production loop starts at 2s and doubles until 60s; split out so the
/// doubling + cap logic can be unit-tested independently.
pub(crate) fn next_backoff(current: Duration, max: Duration) -> Duration {
    (current * 2).min(max)
}

/// Initial backoff duration the reconnect loop uses.
pub(crate) const INITIAL_BACKOFF: Duration = Duration::from_secs(2);

/// Upper bound on the reconnect backoff. We cap here to avoid hour-long
/// reconnect delays after extended outages.
pub(crate) const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Build the "no server URL provided" log line emitted by `run_signaling`
/// on the empty-URL early-return path. Pure helper for the log-format pin.
pub(crate) fn no_server_url_message() -> &'static str {
    "No server URL provided, sleeping forever"
}

/// Build the "connecting to signaling server" log line. Pure helper —
/// pins the format so a future log-format change is caught by a test.
pub(crate) fn connecting_message(server_url: &str) -> String {
    format!("Connecting to signaling server: {server_url}")
}

/// Build the "signaling connection closed cleanly" log line. Pure
/// constant helper that pins the message.
pub(crate) fn closed_cleanly_message() -> &'static str {
    "Signaling connection closed cleanly"
}

/// Build the "signaling connection error" warn line. Pure helper.
pub(crate) fn connection_error_message(err: &str) -> String {
    format!("Signaling connection error: {err}")
}

/// Build the "reconnecting in N seconds" info line. Pure helper.
pub(crate) fn reconnecting_message(secs: u64) -> String {
    format!("Reconnecting in {secs} seconds...")
}

async fn connect_and_handle(
    ctx: &SignalingCtx<'_>,
    ws_outbox_rx: &mut mpsc::Receiver<tokio_tungstenite::tungstenite::Message>,
) -> anyhow::Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let url = build_agent_ws_url(ctx.server_url, ctx.session_id, ctx.agent_token);

    let connector = build_tls_connector(ctx.tls_cert_path);
    let mut ws_config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
    ws_config.max_message_size = Some(2 * 1024 * 1024); // 2MB, matching server
    let (ws_stream, _) = tokio_tungstenite::connect_async_tls_with_config(
        &url,
        Some(ws_config),
        false,
        Some(connector),
    )
    .await
    .context("WebSocket connection failed")?;

    info!("Connected to signaling server");
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    // On reconnect: reset encoder for fresh IDR, clear backgrounded state
    let _ = ctx.capture_cmd_tx.send(CaptureCommand::ResetEncoder);
    ctx.force_keyframe.store(true, Ordering::Relaxed);
    ctx.tab_backgrounded.store(false, Ordering::Relaxed);

    loop {
        tokio::select! {
            // Biased: prioritize incoming server messages (input events,
            // visibility changes, shutdown) over outgoing video/audio frames.
            // Without this, the outbox arm fires at 120fps and can starve
            // incoming messages — causing missed keyframe requests on
            // browser reconnect.
            biased;

            // Incoming messages from server
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match parse_server_text(&text) {
                            ServerTextOutcome::Input(event) => {
                                (ctx.input_callback)(event);
                            }
                            ServerTextOutcome::Shutdown => {
                                info!("Received shutdown command");
                                return Ok(());
                            }
                            ServerTextOutcome::Invalid(e) => {
                                warn!("Invalid message from server: {e}");
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        return Ok(());
                    }
                    Some(Err(e)) => {
                        return Err(e.into());
                    }
                    _ => {}
                }
            }
            // Outgoing messages from video/audio/clipboard/cursor tasks
            Some(msg) = ws_outbox_rx.recv() => {
                ws_tx.send(msg).await?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Once;

    static CRYPTO_INIT: Once = Once::new();

    /// Install the rustls default crypto provider once per test process.
    /// Required for `ClientConfig::builder()` to work without panicking.
    fn ensure_crypto_provider() {
        CRYPTO_INIT.call_once(|| {
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
    }

    /// Generate a self-signed PEM cert/key pair for tests. Returns the cert PEM
    /// as a String so it can be written to a file and passed to
    /// `build_tls_connector`.
    fn make_self_signed_cert_pem() -> String {
        let cert = rcgen::generate_simple_self_signed(vec!["test.local".to_string()]).unwrap();
        cert.cert.pem()
    }

    #[test]
    fn build_tls_connector_works_without_pinned_cert() {
        ensure_crypto_provider();
        let connector = build_tls_connector(None);
        // We can't introspect a Connector easily; just verify it's the Rustls variant.
        match connector {
            tokio_tungstenite::Connector::Rustls(_) => {}
            _ => panic!("Expected Rustls connector"),
        }
    }

    #[test]
    fn build_tls_connector_loads_pinned_cert_from_file() {
        ensure_crypto_provider();
        let dir =
            std::env::temp_dir().join(format!("beam-signaling-cert-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pem_path = dir.join("test-cert.pem");
        std::fs::write(&pem_path, make_self_signed_cert_pem()).unwrap();

        let connector = build_tls_connector(Some(pem_path.to_str().unwrap()));
        match connector {
            tokio_tungstenite::Connector::Rustls(_) => {}
            _ => panic!("Expected Rustls connector"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_tls_connector_falls_back_when_cert_file_missing() {
        ensure_crypto_provider();
        // Missing file logs a warning and falls back to system roots; should
        // still return a Connector without panicking.
        let connector = build_tls_connector(Some("/tmp/beam-nonexistent-cert-file.pem"));
        match connector {
            tokio_tungstenite::Connector::Rustls(_) => {}
            _ => panic!("Expected Rustls connector"),
        }
    }

    #[test]
    fn build_tls_connector_falls_back_when_cert_file_is_invalid_pem() {
        ensure_crypto_provider();
        let dir = std::env::temp_dir().join(format!(
            "beam-signaling-invalid-cert-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let pem_path = dir.join("not-a-cert.pem");
        std::fs::write(&pem_path, b"this is not a pem file").unwrap();

        // Should not panic; falls back to system roots.
        let connector = build_tls_connector(Some(pem_path.to_str().unwrap()));
        match connector {
            tokio_tungstenite::Connector::Rustls(_) => {}
            _ => panic!("Expected Rustls connector"),
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Helper to build a no-op InputCallback for tests that need a
    /// SignalingCtx but don't fire input events.
    fn noop_input_callback() -> Arc<dyn Fn(InputEvent) + Send + Sync> {
        Arc::new(|_event| {})
    }

    #[tokio::test]
    async fn run_signaling_returns_immediately_when_server_url_empty() {
        // The function logs and parks on std::future::pending(). Wrap with a
        // short timeout: if the empty-URL branch is taken, the future will
        // sit forever; the timeout fires and we know the early-return arm ran.
        ensure_crypto_provider();
        let force_kf = Arc::new(AtomicBool::new(false));
        let tab_bg = Arc::new(AtomicBool::new(false));
        let (cmd_tx, _cmd_rx) = std::sync::mpsc::channel::<CaptureCommand>();
        let ctx = SignalingCtx {
            server_url: "",
            session_id: Uuid::new_v4(),
            agent_token: None,
            tls_cert_path: None,
            force_keyframe: force_kf,
            input_callback: noop_input_callback(),
            capture_cmd_tx: &cmd_tx,
            tab_backgrounded: tab_bg,
        };
        let (_tx, mut rx) = mpsc::channel::<tokio_tungstenite::tungstenite::Message>(1);

        // The empty-URL branch parks on pending(). Confirm that's the path by
        // observing the timeout fires before the loop would otherwise complete.
        let result =
            tokio::time::timeout(Duration::from_millis(100), run_signaling(&ctx, &mut rx)).await;
        assert!(
            result.is_err(),
            "Empty server_url should park on pending(), causing the timeout"
        );
    }

    #[test]
    fn signaling_ctx_carries_all_fields() {
        // Smoke test: construct the SignalingCtx with every field populated and
        // verify each field reads back cleanly. Locks the struct shape.
        ensure_crypto_provider();
        let id = Uuid::new_v4();
        let force_kf = Arc::new(AtomicBool::new(true));
        let tab_bg = Arc::new(AtomicBool::new(true));
        let (cmd_tx, _cmd_rx) = std::sync::mpsc::channel::<CaptureCommand>();
        let ctx = SignalingCtx {
            server_url: "wss://example.test",
            session_id: id,
            agent_token: Some("agent-token-abc"),
            tls_cert_path: Some("/tmp/cert.pem"),
            force_keyframe: Arc::clone(&force_kf),
            input_callback: noop_input_callback(),
            capture_cmd_tx: &cmd_tx,
            tab_backgrounded: Arc::clone(&tab_bg),
        };
        assert_eq!(ctx.server_url, "wss://example.test");
        assert_eq!(ctx.session_id, id);
        assert_eq!(ctx.agent_token, Some("agent-token-abc"));
        assert_eq!(ctx.tls_cert_path, Some("/tmp/cert.pem"));
        assert!(ctx.force_keyframe.load(Ordering::Relaxed));
        assert!(ctx.tab_backgrounded.load(Ordering::Relaxed));
    }

    // --- build_agent_ws_url ---

    #[test]
    fn ws_url_without_token_has_no_query_string() {
        let id = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
        let url = build_agent_ws_url("wss://example.test", id, None);
        assert_eq!(
            url,
            "wss://example.test/ws/agent/00000000-0000-0000-0000-000000000001"
        );
        assert!(!url.contains('?'));
    }

    #[test]
    fn ws_url_with_token_appends_query_string() {
        let id = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
        let url = build_agent_ws_url("wss://example.test", id, Some("abc123"));
        assert_eq!(
            url,
            "wss://example.test/ws/agent/00000000-0000-0000-0000-000000000002?token=abc123"
        );
    }

    #[test]
    fn ws_url_url_encodes_token_special_chars() {
        // Tokens may contain '/', '=', '+', '&' (typical for base64 / URL-safe).
        // The URL-encoded form must NOT introduce spurious query separators
        // into the URL.
        let id = Uuid::nil();
        let url = build_agent_ws_url("wss://example.test", id, Some("a/b=c&d+e"));
        // The encoded token portion comes after `?token=`. Verify each suspect
        // character is percent-encoded.
        assert!(url.contains("?token="));
        // '/' -> %2F
        assert!(url.contains("%2F"), "'/' should be percent-encoded: {url}");
        // '=' -> %3D
        assert!(url.contains("%3D"), "'=' should be percent-encoded: {url}");
        // '&' -> %26 — critical so it doesn't introduce a new query param
        assert!(url.contains("%26"), "'&' should be percent-encoded: {url}");
        // '+' -> %2B
        assert!(url.contains("%2B"), "'+' should be percent-encoded: {url}");
    }

    #[test]
    fn ws_url_handles_empty_token() {
        // Empty token still yields a "?token=" suffix (rather than no query
        // string), so the server can distinguish "no token provided" from
        // "token field is empty".
        let id = Uuid::nil();
        let url = build_agent_ws_url("wss://example.test", id, Some(""));
        assert!(url.ends_with("?token="));
    }

    #[test]
    fn ws_url_preserves_server_url_path_prefix() {
        // If the server URL includes a path prefix (e.g., wss://h/proxied),
        // build_agent_ws_url should preserve it and append the /ws/agent/ path.
        let id = Uuid::nil();
        let url = build_agent_ws_url("wss://example.test/proxied", id, None);
        assert!(url.starts_with("wss://example.test/proxied/ws/agent/"));
    }

    #[test]
    fn ws_url_with_port_preserves_port() {
        let id = Uuid::nil();
        let url = build_agent_ws_url("wss://example.test:8443", id, None);
        assert!(url.starts_with("wss://example.test:8443/ws/agent/"));
    }

    #[test]
    fn ws_url_uses_ws_path() {
        // The /ws/agent/ path is the contract with beam-server's signaling
        // route registration; if it ever drifts, this test catches it.
        let id = Uuid::nil();
        let url = build_agent_ws_url("wss://example.test", id, None);
        assert!(url.contains("/ws/agent/"));
    }

    // --- parse_server_text ---

    #[test]
    fn parse_server_text_shutdown_command() {
        let text = serde_json::to_string(&AgentCommand::Shutdown).unwrap();
        match parse_server_text(&text) {
            ServerTextOutcome::Shutdown => {}
            other => panic!("Expected Shutdown, got {other:?}"),
        }
    }

    #[test]
    fn parse_server_text_input_command() {
        // KeyDown event wrapped in AgentCommand::Input.
        let cmd = AgentCommand::Input(InputEvent::Key { c: 65, d: true });
        let text = serde_json::to_string(&cmd).unwrap();
        match parse_server_text(&text) {
            ServerTextOutcome::Input(InputEvent::Key { c, d }) => {
                assert_eq!(c, 65);
                assert!(d);
            }
            other => panic!("Expected Input::Key, got {other:?}"),
        }
    }

    #[test]
    fn parse_server_text_invalid_json_returns_invalid() {
        match parse_server_text("not json at all") {
            ServerTextOutcome::Invalid(msg) => {
                assert!(!msg.is_empty(), "Error message should not be empty");
            }
            other => panic!("Expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn parse_server_text_empty_input_returns_invalid() {
        match parse_server_text("") {
            ServerTextOutcome::Invalid(_) => {}
            other => panic!("Expected Invalid for empty, got {other:?}"),
        }
    }

    #[test]
    fn parse_server_text_wrong_shape_returns_invalid() {
        // Valid JSON, but not an AgentCommand.
        match parse_server_text(r#"{"foo": "bar"}"#) {
            ServerTextOutcome::Invalid(_) => {}
            other => panic!("Expected Invalid for wrong shape, got {other:?}"),
        }
    }

    #[test]
    fn parse_server_text_resize_input_event_roundtrip() {
        let cmd = AgentCommand::Input(InputEvent::Resize { w: 1920, h: 1080 });
        let text = serde_json::to_string(&cmd).unwrap();
        match parse_server_text(&text) {
            ServerTextOutcome::Input(InputEvent::Resize { w, h }) => {
                assert_eq!(w, 1920);
                assert_eq!(h, 1080);
            }
            other => panic!("Expected Input::Resize, got {other:?}"),
        }
    }

    #[test]
    fn parse_server_text_visibility_input_event_roundtrip() {
        let cmd = AgentCommand::Input(InputEvent::VisibilityState { visible: false });
        let text = serde_json::to_string(&cmd).unwrap();
        match parse_server_text(&text) {
            ServerTextOutcome::Input(InputEvent::VisibilityState { visible }) => {
                assert!(!visible);
            }
            other => panic!("Expected VisibilityState, got {other:?}"),
        }
    }

    // --- next_backoff ---

    #[test]
    fn next_backoff_doubles_current() {
        let result = next_backoff(Duration::from_secs(2), Duration::from_secs(60));
        assert_eq!(result, Duration::from_secs(4));
    }

    #[test]
    fn next_backoff_caps_at_max() {
        // 32 doubled = 64, but max is 60 → cap at 60.
        let result = next_backoff(Duration::from_secs(32), Duration::from_secs(60));
        assert_eq!(result, Duration::from_secs(60));
    }

    #[test]
    fn next_backoff_already_at_max_stays_at_max() {
        // 60 * 2 = 120, capped to 60.
        let result = next_backoff(Duration::from_secs(60), Duration::from_secs(60));
        assert_eq!(result, Duration::from_secs(60));
    }

    #[test]
    fn next_backoff_grows_through_full_sequence() {
        // 2s → 4s → 8s → 16s → 32s → 60s (capped) → 60s (capped)
        let mut backoff = INITIAL_BACKOFF;
        let expected = [4, 8, 16, 32, 60, 60, 60];
        for expected_secs in expected {
            backoff = next_backoff(backoff, MAX_BACKOFF);
            assert_eq!(backoff, Duration::from_secs(expected_secs));
        }
    }

    #[test]
    fn next_backoff_starts_at_2_seconds() {
        assert_eq!(INITIAL_BACKOFF, Duration::from_secs(2));
    }

    #[test]
    fn next_backoff_max_is_60_seconds() {
        assert_eq!(MAX_BACKOFF, Duration::from_secs(60));
    }

    #[test]
    fn next_backoff_with_zero_returns_zero() {
        let result = next_backoff(Duration::from_secs(0), Duration::from_secs(60));
        assert_eq!(result, Duration::from_secs(0));
    }

    // --- Log-message helpers ---

    #[test]
    fn no_server_url_message_static() {
        let m = no_server_url_message();
        assert!(m.contains("No server URL"));
        assert!(m.contains("sleeping"));
    }

    #[test]
    fn connecting_message_includes_url() {
        let m = connecting_message("wss://beam.test:8443");
        assert!(m.contains("Connecting"));
        assert!(m.contains("wss://beam.test:8443"));
    }

    #[test]
    fn connecting_message_handles_empty_url() {
        // The branch with empty URL takes the early-return path, but
        // defensive: the formatter still works.
        let m = connecting_message("");
        assert!(m.starts_with("Connecting"));
    }

    #[test]
    fn closed_cleanly_message_static() {
        let m = closed_cleanly_message();
        assert!(m.contains("closed cleanly"));
        assert!(m.contains("Signaling"));
    }

    #[test]
    fn connection_error_message_includes_err() {
        let m = connection_error_message("connection refused");
        assert!(m.contains("connection refused"));
        assert!(m.contains("Signaling connection error"));
    }

    #[test]
    fn reconnecting_message_includes_seconds() {
        let m = reconnecting_message(30);
        assert!(m.contains("30 seconds"));
        assert!(m.contains("Reconnecting"));
    }

    #[test]
    fn reconnecting_message_handles_zero() {
        let m = reconnecting_message(0);
        assert!(m.contains("0 seconds"));
    }

    #[test]
    fn reconnecting_message_handles_large_value() {
        let m = reconnecting_message(86_400);
        assert!(m.contains("86400 seconds"));
    }
}
