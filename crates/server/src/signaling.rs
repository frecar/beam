use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use beam_protocol::{AgentCommand, FRAME_MAGIC, InputEvent, SignalingMessage};
use bytes::Bytes;
use tokio::sync::{Notify, RwLock, broadcast};
use tokio::time::{Duration, Instant, interval};
use uuid::Uuid;

use crate::client_metrics::ClientMetricsStore;

/// Interval between WebSocket ping frames.
const WS_PING_INTERVAL: Duration = Duration::from_secs(30);

/// Maximum time to wait for a pong response before considering the connection dead.
/// This allows 3 missed pings (3 * 30s = 90s).
const WS_PONG_TIMEOUT: Duration = Duration::from_secs(90);

/// Per-session signaling channel with separate paths for browser→agent and agent→browser.
/// Binary video/audio frames from the agent are relayed via a separate broadcast channel.
pub struct SignalingChannel {
    /// Text messages (input events) from browser, forwarded to agent as AgentCommand::Input
    pub to_agent: broadcast::Sender<AgentCommand>,
    /// Text messages from agent to browser (raw JSON — signaling, clipboard, cursor, file data)
    pub to_browser: broadcast::Sender<String>,
    /// Binary video/audio frames from agent, relayed to browser
    pub video_frames: broadcast::Sender<Bytes>,
    /// Notified when a new browser connects, kicking the previous one.
    /// Only one browser WebSocket per session is supported at a time.
    pub browser_kick: Notify,
}

impl SignalingChannel {
    pub fn new() -> Self {
        let (to_agent, _) = broadcast::channel(64);
        let (to_browser, _) = broadcast::channel(64);
        let (video_frames, _) = broadcast::channel(64);
        Self {
            to_agent,
            to_browser,
            video_frames,
            browser_kick: Notify::new(),
        }
    }
}

/// Registry of active signaling channels keyed by session ID.
pub type ChannelRegistry = Arc<RwLock<HashMap<Uuid, Arc<SignalingChannel>>>>;

/// Create a new empty channel registry.
pub fn new_channel_registry() -> ChannelRegistry {
    Arc::new(RwLock::new(HashMap::new()))
}

/// Extract the 32-bit magic value from the front of a binary frame.
/// Returns `None` if the frame is shorter than 4 bytes.
///
/// Split out so the magic-check branching can be unit-tested without
/// owning an in-flight WebSocket connection.
pub(crate) fn frame_magic(data: &[u8]) -> Option<u32> {
    if data.len() < 4 {
        return None;
    }
    Some(u32::from_le_bytes([data[0], data[1], data[2], data[3]]))
}

/// Check whether a binary frame's magic header matches the expected
/// FRAME_MAGIC constant. Returns `false` for short frames AND for valid
/// frames with the wrong magic (both are dropped before relay).
pub(crate) fn frame_magic_is_valid(data: &[u8]) -> bool {
    frame_magic(data).is_some_and(|m| m == FRAME_MAGIC)
}

/// Outcome of parsing a browser→server text frame.
#[derive(Debug)]
pub(crate) enum BrowserMessageOutcome {
    /// A ClientMetricsPing — reply with a pong (only if metrics are enabled).
    Pong { id: u32, sent_ms: f64 },
    /// A ClientMetrics report — update the client metrics store (only if
    /// metrics are enabled).
    Metrics(beam_protocol::ClientMetricsReport),
    /// A regular input event — wrap as AgentCommand and forward to the agent.
    Forward(AgentCommand),
    /// Malformed JSON or unknown shape — reply with an error frame whose body
    /// is this string.
    InvalidJson(String),
}

/// Parse a browser→server text frame into one of the [`BrowserMessageOutcome`]
/// variants. Split out so the dispatch can be unit-tested without driving a
/// real WebSocket. Error message text is included for diagnostic surfaces.
/// Decide whether the browser-WS loop should log the "first frames"
/// info line for this video frame count. Production logs the first 3
/// frames to confirm the pipeline started, then goes silent. Pure
/// helper around the threshold.
pub(crate) fn should_log_initial_video_frame(count: u64) -> bool {
    count <= 3
}

/// Decide whether the agent-WS pong-timeout has elapsed. Pure helper
/// for the timeout-vs-keepalive decision.
pub(crate) fn pong_timeout_elapsed(elapsed_ms: u64, timeout_ms: u64) -> bool {
    elapsed_ms > timeout_ms
}

/// Build the "browser replaced by new connection" log line. Pure helper.
pub(crate) fn browser_replaced_message() -> &'static str {
    "Browser replaced by new connection"
}

/// Build the "invalid browser message" format string. Pure helper —
/// includes the error so the browser sees what went wrong.
pub(crate) fn invalid_browser_message_format(err: &str) -> String {
    format!("Invalid message format: {err}")
}

pub(crate) fn parse_browser_text(text: &str) -> BrowserMessageOutcome {
    match serde_json::from_str::<InputEvent>(text) {
        Ok(InputEvent::ClientMetricsPing { id, sent_ms }) => {
            BrowserMessageOutcome::Pong { id, sent_ms }
        }
        Ok(InputEvent::ClientMetrics(report)) => BrowserMessageOutcome::Metrics(report),
        Ok(event) => BrowserMessageOutcome::Forward(AgentCommand::Input(event)),
        Err(e) => BrowserMessageOutcome::InvalidJson(e.to_string()),
    }
}

/// Outcome of parsing a server→agent (incoming-from-server) text frame.
#[cfg(test)]
#[derive(Debug)]
pub(crate) enum AgentTextOutcome {
    /// Forward the input event to the local injector.
    Input(InputEvent),
    /// Shut down the agent (clean exit).
    Shutdown,
    /// Malformed JSON or unknown shape — log + ignore.
    Invalid(#[allow(dead_code)] String),
}

/// Parse an agent-side incoming text frame as an [`AgentCommand`]. Split out
/// for unit testing.
#[cfg(test)]
pub(crate) fn parse_agent_command_text(text: &str) -> AgentTextOutcome {
    match serde_json::from_str::<AgentCommand>(text) {
        Ok(AgentCommand::Input(event)) => AgentTextOutcome::Input(event),
        Ok(AgentCommand::Shutdown) => AgentTextOutcome::Shutdown,
        Err(e) => AgentTextOutcome::Invalid(e.to_string()),
    }
}

/// Get or create a signaling channel for the given session.
pub async fn get_or_create_channel(
    registry: &ChannelRegistry,
    session_id: Uuid,
) -> Arc<SignalingChannel> {
    {
        let channels = registry.read().await;
        if let Some(ch) = channels.get(&session_id) {
            return Arc::clone(ch);
        }
    }

    let mut channels = registry.write().await;
    channels
        .entry(session_id)
        .or_insert_with(|| Arc::new(SignalingChannel::new()))
        .clone()
}

/// Remove a signaling channel when a session is destroyed.
pub async fn remove_channel(registry: &ChannelRegistry, session_id: Uuid) {
    let mut channels = registry.write().await;
    channels.remove(&session_id);
    tracing::debug!(%session_id, "Signaling channel removed");
}

/// Handle a WebSocket connection from a **browser** client.
///
/// Browser sends text → parsed as InputEvent, wrapped in AgentCommand::Input, sent to agent.
/// Browser receives ← text messages (signaling, clipboard, cursor) + binary video/audio frames.
///
/// Only one browser per session at a time. Connecting a new browser
/// kicks the previous one with close code 4001 ("replaced").
pub async fn handle_browser_ws(
    mut socket: WebSocket,
    session_id: Uuid,
    registry: ChannelRegistry,
    client_metrics_enabled: bool,
    client_metrics: Arc<ClientMetricsStore>,
) {
    tracing::info!(%session_id, "Browser WebSocket upgrade request");
    let channel = get_or_create_channel(&registry, session_id).await;

    // Kick any existing browser for this session
    tracing::info!(%session_id, "Kicking any existing browsers for this session");
    channel.browser_kick.notify_waiters();

    // Notify agent that a browser is now connected — clears backgrounded mode,
    // forces a keyframe, and wakes the capture thread so the first frame is
    // sent promptly. The agent uses this to reset the encoder so the browser's
    // decoder can start decoding immediately (it needs a keyframe/IDR).
    let receivers = channel
        .to_agent
        .send(AgentCommand::Input(InputEvent::VisibilityState {
            visible: true,
        }))
        .unwrap_or(0);
    if receivers == 0 {
        tracing::warn!(%session_id, "No agent receivers for visibility notification — keyframe may not be sent");
    } else {
        tracing::info!(%session_id, receivers, "Sent visibility notification to agent for keyframe");
    }

    let mut from_agent = channel.to_browser.subscribe();
    let mut from_agent_video = channel.video_frames.subscribe();
    // Register our own kick listener AFTER kicking the old browser
    let kicked = channel.browser_kick.notified();
    tokio::pin!(kicked);

    // Ping/pong keepalive state
    let mut ping_interval = interval(WS_PING_INTERVAL);
    ping_interval.tick().await; // consume the immediate first tick
    let mut last_pong = Instant::now();

    tracing::info!(%session_id, "Browser WebSocket connected");
    let mut video_frames_relayed: u64 = 0;

    loop {
        tokio::select! {
            // Kicked by a newer browser connection
            _ = &mut kicked => {
                tracing::info!(%session_id, "{}", browser_replaced_message());
                // Tell the old browser why it's being disconnected
                let msg = SignalingMessage::Error {
                    message: "replaced".to_string(),
                };
                if let Ok(json) = serde_json::to_string(&msg) {
                    let _ = socket.send(Message::Text(json.into())).await;
                }
                break;
            }
            // Send periodic WebSocket ping frames
            _ = ping_interval.tick() => {
                if pong_timeout_elapsed(
                    last_pong.elapsed().as_millis() as u64,
                    WS_PONG_TIMEOUT.as_millis() as u64,
                ) {
                    tracing::debug!(%session_id, "Browser WebSocket ping timeout, closing");
                    break;
                }
                if socket.send(Message::Ping(vec![].into())).await.is_err() {
                    tracing::debug!(%session_id, "Browser WebSocket ping send failed");
                    break;
                }
            }
            // Forward agent text messages (raw JSON) to browser
            result = from_agent.recv() => {
                let text = match result {
                    Ok(t) => t,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(%session_id, skipped = n, "Browser signaling consumer lagged");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                if socket.send(Message::Text(text.into())).await.is_err() {
                    tracing::debug!(%session_id, "Browser WebSocket send failed");
                    break;
                }
            }
            // Forward agent binary frames (video/audio) to browser
            result = from_agent_video.recv() => {
                match result {
                    Ok(frame) => {
                        video_frames_relayed += 1;
                        if should_log_initial_video_frame(video_frames_relayed) {
                            tracing::info!(%session_id, size = frame.len(), frame = video_frames_relayed, "Relaying binary frame to browser");
                        }
                        if socket.send(Message::Binary(frame.to_vec().into())).await.is_err() {
                            tracing::debug!(%session_id, "Browser WebSocket binary send failed");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(%session_id, skipped = n, "Browser video consumer lagged — requesting keyframe");
                        // Dropped frames likely included the IDR keyframe.
                        // Request a new one so the browser decoder can recover.
                        let _ = channel.to_agent.send(AgentCommand::Input(InputEvent::VisibilityState { visible: true }));
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            // Receive messages from browser and forward to agent
            Some(result) = socket.recv() => {
                match result {
                    Ok(Message::Text(text)) => {
                        match parse_browser_text(&text) {
                            BrowserMessageOutcome::Pong { id, sent_ms } => {
                                if client_metrics_enabled {
                                    let msg = SignalingMessage::MetricsPong { id, sent_ms };
                                    if let Ok(json) = serde_json::to_string(&msg) {
                                        let _ = socket.send(Message::Text(json.into())).await;
                                    }
                                }
                            }
                            BrowserMessageOutcome::Metrics(report) => {
                                if client_metrics_enabled {
                                    client_metrics.update(session_id, report);
                                }
                            }
                            BrowserMessageOutcome::Forward(cmd) => {
                                if let Err(e) = channel.to_agent.send(cmd) {
                                    tracing::warn!(%session_id, "No agent listening for input: {e}");
                                }
                            }
                            BrowserMessageOutcome::InvalidJson(e) => {
                                tracing::warn!(%session_id, "Invalid browser message: {e}");
                                let err = SignalingMessage::Error {
                                    message: invalid_browser_message_format(&e),
                                };
                                let json = serde_json::to_string(&err).unwrap_or_default();
                                let _ = socket.send(Message::Text(json.into())).await;
                            }
                        }
                    }
                    Ok(Message::Pong(_)) => {
                        last_pong = Instant::now();
                    }
                    Ok(Message::Close(_)) => {
                        tracing::info!(%session_id, "Browser WebSocket closed");
                        break;
                    }
                    Err(e) => {
                        tracing::debug!(%session_id, "Browser WebSocket error: {e}");
                        break;
                    }
                    _ => {}
                }
            }
            else => break,
        }
    }

    tracing::info!(%session_id, "Browser WebSocket disconnected");
}

/// Handle a WebSocket connection from a **beam-agent**.
///
/// Agent sends text → forwarded to browser as-is (raw JSON relay).
/// Agent sends binary → validated and relayed to browser as video/audio frames.
/// Agent receives ← AgentCommand (input events + shutdown).
pub async fn handle_agent_ws(mut socket: WebSocket, session_id: Uuid, registry: ChannelRegistry) {
    tracing::info!(%session_id, "Agent WebSocket upgrade request");
    let channel = get_or_create_channel(&registry, session_id).await;
    let mut from_browser = channel.to_agent.subscribe();

    // Ping/pong keepalive state
    let mut ping_interval = interval(WS_PING_INTERVAL);
    ping_interval.tick().await; // consume the immediate first tick
    let mut last_pong = Instant::now();

    tracing::info!(%session_id, "Agent WebSocket connected");

    loop {
        tokio::select! {
            // Send periodic WebSocket ping frames
            _ = ping_interval.tick() => {
                if pong_timeout_elapsed(
                    last_pong.elapsed().as_millis() as u64,
                    WS_PONG_TIMEOUT.as_millis() as u64,
                ) {
                    tracing::debug!(%session_id, "Agent WebSocket ping timeout, closing");
                    break;
                }
                if socket.send(Message::Ping(vec![].into())).await.is_err() {
                    tracing::debug!(%session_id, "Agent WebSocket ping send failed");
                    break;
                }
            }
            // Forward browser input/commands to agent
            result = from_browser.recv() => {
                let cmd = match result {
                    Ok(m) => m,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(%session_id, skipped = n, "Agent consumer lagged, commands dropped");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let json = match serde_json::to_string(&cmd) {
                    Ok(j) => j,
                    Err(e) => {
                        tracing::error!("Failed to serialize agent command: {e}");
                        continue;
                    }
                };
                if socket.send(Message::Text(json.into())).await.is_err() {
                    tracing::debug!(%session_id, "Agent WebSocket send failed");
                    break;
                }
            }
            // Receive messages from agent
            Some(result) = socket.recv() => {
                match result {
                    Ok(Message::Text(text)) => {
                        // Relay agent text messages to browser as-is (raw JSON).
                        // This carries signaling (SessionReady, Error) plus data
                        // messages (clipboard, cursor shape, file transfer).
                        tracing::debug!(%session_id, "Agent → Browser text relay");
                        if let Err(e) = channel.to_browser.send(text.to_string()) {
                            tracing::warn!(%session_id, "No browser listening: {e}");
                        }
                    }
                    Ok(Message::Binary(data)) => {
                        // Binary frames: validate magic header, relay to browser
                        let len = data.len();
                        if frame_magic_is_valid(&data) {
                            let receivers = channel.video_frames.receiver_count();
                            match channel.video_frames.send(Bytes::from(data.to_vec())) {
                                Ok(n) => {
                                    tracing::trace!(%session_id, len, receivers, sent_to = n, "Relayed binary frame");
                                }
                                Err(e) => {
                                    tracing::debug!(%session_id, len, receivers, "No browser listening for video: {e}");
                                }
                            }
                        } else if let Some(magic) = frame_magic(&data) {
                            tracing::warn!(%session_id, "Agent sent binary with bad magic: 0x{magic:08x}");
                        }
                    }
                    Ok(Message::Pong(_)) => {
                        last_pong = Instant::now();
                    }
                    Ok(Message::Close(_)) => {
                        tracing::info!(%session_id, "Agent WebSocket closed");
                        break;
                    }
                    Err(e) => {
                        tracing::debug!(%session_id, "Agent WebSocket error: {e}");
                        break;
                    }
                    _ => {}
                }
            }
            else => break,
        }
    }

    tracing::info!(%session_id, "Agent WebSocket disconnected");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn get_or_create_channel_creates_new() {
        let registry = new_channel_registry();
        let id = Uuid::new_v4();
        let channel = get_or_create_channel(&registry, id).await;

        // Verify it's in the registry
        let channels = registry.read().await;
        assert!(channels.contains_key(&id));
        assert!(Arc::ptr_eq(channels.get(&id).unwrap(), &channel));
    }

    #[tokio::test]
    async fn get_or_create_channel_returns_existing() {
        let registry = new_channel_registry();
        let id = Uuid::new_v4();
        let ch1 = get_or_create_channel(&registry, id).await;
        let ch2 = get_or_create_channel(&registry, id).await;
        assert!(Arc::ptr_eq(&ch1, &ch2));
    }

    #[tokio::test]
    async fn get_or_create_different_ids_different_channels() {
        let registry = new_channel_registry();
        let id1 = Uuid::new_v4();
        let id2 = Uuid::new_v4();
        let ch1 = get_or_create_channel(&registry, id1).await;
        let ch2 = get_or_create_channel(&registry, id2).await;
        assert!(!Arc::ptr_eq(&ch1, &ch2));
    }

    #[tokio::test]
    async fn remove_channel_clears_registry() {
        let registry = new_channel_registry();
        let id = Uuid::new_v4();
        let _channel = get_or_create_channel(&registry, id).await;
        remove_channel(&registry, id).await;

        let channels = registry.read().await;
        assert!(channels.is_empty());
    }

    #[tokio::test]
    async fn remove_nonexistent_channel_no_panic() {
        let registry = new_channel_registry();
        remove_channel(&registry, Uuid::new_v4()).await;
        // Should not panic
    }

    #[tokio::test]
    async fn remove_then_recreate_gives_fresh_channel() {
        let registry = new_channel_registry();
        let id = Uuid::new_v4();
        let ch1 = get_or_create_channel(&registry, id).await;
        remove_channel(&registry, id).await;
        let ch2 = get_or_create_channel(&registry, id).await;
        assert!(!Arc::ptr_eq(&ch1, &ch2));
    }

    #[tokio::test]
    async fn video_frame_relay() {
        let channel = SignalingChannel::new();
        let mut rx = channel.video_frames.subscribe();
        let payload = Bytes::from_static(b"fake-frame-data");
        channel.video_frames.send(payload.clone()).unwrap();

        let received = rx.recv().await.unwrap();
        assert_eq!(received, payload);
    }

    #[tokio::test]
    async fn video_frame_lagged_drops() {
        let channel = SignalingChannel::new();
        let mut rx = channel.video_frames.subscribe();

        // Channel capacity is 64; send 70 frames to trigger lag
        for i in 0..70u8 {
            let _ = channel.video_frames.send(Bytes::from(vec![i]));
        }

        // First recv should report lag
        match rx.recv().await {
            Err(broadcast::error::RecvError::Lagged(n)) => {
                assert!(n > 0, "expected some lagged frames, got 0");
            }
            other => panic!("expected Lagged, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn browser_kick_notification() {
        let channel = SignalingChannel::new();
        let notified = channel.browser_kick.notified();

        // Kick fires after notified() is registered
        channel.browser_kick.notify_waiters();

        // The future should complete immediately
        tokio::time::timeout(Duration::from_millis(100), notified)
            .await
            .expect("browser_kick notification should resolve");
    }

    #[tokio::test]
    async fn text_relay_browser_to_agent() {
        let channel = SignalingChannel::new();
        let mut rx = channel.to_agent.subscribe();

        let cmd = AgentCommand::Shutdown;
        channel.to_agent.send(cmd.clone()).unwrap();

        let received = rx.recv().await.unwrap();
        assert!(matches!(received, AgentCommand::Shutdown));
    }

    #[tokio::test]
    async fn text_relay_agent_to_browser() {
        let channel = SignalingChannel::new();
        let mut rx = channel.to_browser.subscribe();

        let msg = r#"{"type":"session_ready"}"#.to_string();
        channel.to_browser.send(msg.clone()).unwrap();

        let received = rx.recv().await.unwrap();
        assert_eq!(received, msg);
    }

    #[tokio::test]
    async fn browser_connect_sends_visibility_to_agent() {
        // Simulates the browser connect flow: server sends VisibilityState{visible:true}
        // to the agent via the to_agent broadcast channel.
        let channel = SignalingChannel::new();
        let mut rx = channel.to_agent.subscribe();

        // This is what handle_browser_ws does on connect
        let receivers = channel
            .to_agent
            .send(AgentCommand::Input(InputEvent::VisibilityState {
                visible: true,
            }))
            .unwrap_or(0);

        assert_eq!(receivers, 1, "Should have exactly one subscriber");

        let received = rx.recv().await.unwrap();
        match received {
            AgentCommand::Input(InputEvent::VisibilityState { visible }) => {
                assert!(visible, "Should be visible=true for reconnect");
            }
            _ => panic!("Expected VisibilityState, got {:?}", received),
        }
    }

    #[tokio::test]
    async fn visibility_notification_returns_zero_without_subscribers() {
        // When the agent is not connected, the broadcast has no subscribers.
        // The server should detect this (receivers == 0).
        let channel = SignalingChannel::new();
        // Don't subscribe — simulates agent not connected

        let receivers = channel
            .to_agent
            .send(AgentCommand::Input(InputEvent::VisibilityState {
                visible: true,
            }))
            .unwrap_or(0);

        assert_eq!(
            receivers, 0,
            "Should be zero receivers when agent not connected"
        );
    }

    #[tokio::test]
    async fn visibility_notification_reaches_multiple_subscribers() {
        // Even though only one agent handler subscribes in practice,
        // verify broadcast semantics work correctly.
        let channel = SignalingChannel::new();
        let mut rx1 = channel.to_agent.subscribe();
        let mut rx2 = channel.to_agent.subscribe();

        let receivers = channel
            .to_agent
            .send(AgentCommand::Input(InputEvent::VisibilityState {
                visible: true,
            }))
            .unwrap_or(0);

        assert_eq!(receivers, 2);
        assert!(matches!(
            rx1.recv().await.unwrap(),
            AgentCommand::Input(InputEvent::VisibilityState { visible: true })
        ));
        assert!(matches!(
            rx2.recv().await.unwrap(),
            AgentCommand::Input(InputEvent::VisibilityState { visible: true })
        ));
    }

    #[tokio::test]
    async fn channel_reuse_preserves_agent_subscription() {
        // When browser reconnects, the channel is reused (not recreated).
        // The agent's subscription on to_agent must still work.
        let registry = new_channel_registry();
        let id = Uuid::new_v4();

        let ch1 = get_or_create_channel(&registry, id).await;
        let mut agent_rx = ch1.to_agent.subscribe();

        // Second browser connect reuses the same channel
        let ch2 = get_or_create_channel(&registry, id).await;
        assert!(Arc::ptr_eq(&ch1, &ch2));

        // Send visibility notification via the reused channel
        ch2.to_agent
            .send(AgentCommand::Input(InputEvent::VisibilityState {
                visible: true,
            }))
            .unwrap();

        // Agent should receive it
        let received = agent_rx.recv().await.unwrap();
        assert!(matches!(
            received,
            AgentCommand::Input(InputEvent::VisibilityState { visible: true })
        ));
    }

    // --- frame_magic / frame_magic_is_valid ---

    #[test]
    fn frame_magic_returns_none_for_empty() {
        assert_eq!(frame_magic(&[]), None);
        assert!(!frame_magic_is_valid(&[]));
    }

    #[test]
    fn frame_magic_returns_none_for_short_frame() {
        // Anything shorter than 4 bytes can't possibly carry a u32 magic.
        for n in 0..4 {
            let bytes = vec![0u8; n];
            assert_eq!(frame_magic(&bytes), None);
            assert!(!frame_magic_is_valid(&bytes));
        }
    }

    #[test]
    fn frame_magic_decodes_little_endian() {
        // Magic 0x04030201 in little-endian = bytes [0x01, 0x02, 0x03, 0x04]
        let bytes = [0x01, 0x02, 0x03, 0x04];
        assert_eq!(frame_magic(&bytes), Some(0x04030201));
    }

    #[test]
    fn frame_magic_ignores_trailing_bytes() {
        // The function only inspects the first 4 bytes.
        let mut frame = vec![0x01, 0x02, 0x03, 0x04];
        frame.extend([0xAA, 0xBB, 0xCC]);
        assert_eq!(frame_magic(&frame), Some(0x04030201));
    }

    #[test]
    fn frame_magic_is_valid_accepts_correct_constant() {
        // Build a frame whose first 4 bytes are exactly FRAME_MAGIC LE.
        let magic = FRAME_MAGIC.to_le_bytes();
        let frame: Vec<u8> = magic.iter().copied().chain(vec![0u8; 20]).collect();
        assert!(frame_magic_is_valid(&frame));
    }

    #[test]
    fn frame_magic_is_valid_rejects_wrong_magic() {
        let wrong = (FRAME_MAGIC.wrapping_add(1)).to_le_bytes();
        let frame: Vec<u8> = wrong.iter().copied().chain(vec![0u8; 20]).collect();
        assert!(!frame_magic_is_valid(&frame));
    }

    #[test]
    fn frame_magic_is_valid_rejects_zero_magic() {
        // All-zero header is the most common garbage frame and must be rejected.
        let frame = vec![0u8; 24];
        assert!(!frame_magic_is_valid(&frame));
    }

    #[test]
    fn frame_magic_is_valid_handles_min_length() {
        // Exactly 4 bytes of magic + nothing else is a malformed but
        // non-rejected-by-magic frame. The full validation (header parsing)
        // happens later in the relay.
        let magic = FRAME_MAGIC.to_le_bytes();
        assert!(frame_magic_is_valid(&magic));
    }

    // --- parse_browser_text ---

    #[test]
    fn parse_browser_text_metrics_ping_returns_pong() {
        let ping = InputEvent::ClientMetricsPing {
            id: 42,
            sent_ms: 1000.0,
        };
        let text = serde_json::to_string(&ping).unwrap();
        match parse_browser_text(&text) {
            BrowserMessageOutcome::Pong { id, sent_ms } => {
                assert_eq!(id, 42);
                assert!((sent_ms - 1000.0).abs() < f64::EPSILON);
            }
            other => panic!("Expected Pong, got {other:?}"),
        }
    }

    #[test]
    fn parse_browser_text_input_event_returns_forward() {
        let event = InputEvent::Key { c: 65, d: true };
        let text = serde_json::to_string(&event).unwrap();
        match parse_browser_text(&text) {
            BrowserMessageOutcome::Forward(AgentCommand::Input(InputEvent::Key { c, d })) => {
                assert_eq!(c, 65);
                assert!(d);
            }
            other => panic!("Expected Forward, got {other:?}"),
        }
    }

    #[test]
    fn parse_browser_text_invalid_json_returns_invalid_json() {
        match parse_browser_text("not even close to json") {
            BrowserMessageOutcome::InvalidJson(msg) => {
                assert!(!msg.is_empty(), "Error message must not be empty");
            }
            other => panic!("Expected InvalidJson, got {other:?}"),
        }
    }

    #[test]
    fn parse_browser_text_empty_string_returns_invalid_json() {
        match parse_browser_text("") {
            BrowserMessageOutcome::InvalidJson(_) => {}
            other => panic!("Expected InvalidJson for empty, got {other:?}"),
        }
    }

    #[test]
    fn parse_browser_text_unknown_event_type_returns_invalid_json() {
        match parse_browser_text(r#"{"t":"totally_unknown_event"}"#) {
            BrowserMessageOutcome::InvalidJson(_) => {}
            other => panic!("Expected InvalidJson, got {other:?}"),
        }
    }

    #[test]
    fn parse_browser_text_mouse_move_returns_forward() {
        let event = InputEvent::MouseMove { x: 0.5, y: 0.25 };
        let text = serde_json::to_string(&event).unwrap();
        match parse_browser_text(&text) {
            BrowserMessageOutcome::Forward(AgentCommand::Input(InputEvent::MouseMove { x, y })) => {
                assert!((x - 0.5).abs() < f64::EPSILON);
                assert!((y - 0.25).abs() < f64::EPSILON);
            }
            other => panic!("Expected Forward MouseMove, got {other:?}"),
        }
    }

    #[test]
    fn parse_browser_text_resize_event_returns_forward() {
        let event = InputEvent::Resize { w: 2560, h: 1440 };
        let text = serde_json::to_string(&event).unwrap();
        match parse_browser_text(&text) {
            BrowserMessageOutcome::Forward(AgentCommand::Input(InputEvent::Resize { w, h })) => {
                assert_eq!(w, 2560);
                assert_eq!(h, 1440);
            }
            other => panic!("Expected Forward Resize, got {other:?}"),
        }
    }

    #[test]
    fn parse_browser_text_metrics_report_returns_metrics() {
        let report = beam_protocol::ClientMetricsReport::default();
        let event = InputEvent::ClientMetrics(report);
        let text = serde_json::to_string(&event).unwrap();
        match parse_browser_text(&text) {
            BrowserMessageOutcome::Metrics(_) => {}
            other => panic!("Expected Metrics, got {other:?}"),
        }
    }

    // --- parse_agent_command_text ---

    #[test]
    fn parse_agent_command_text_shutdown() {
        let cmd = AgentCommand::Shutdown;
        let text = serde_json::to_string(&cmd).unwrap();
        match parse_agent_command_text(&text) {
            AgentTextOutcome::Shutdown => {}
            other => panic!("Expected Shutdown, got {other:?}"),
        }
    }

    #[test]
    fn parse_agent_command_text_input_event() {
        let cmd = AgentCommand::Input(InputEvent::Key { c: 100, d: false });
        let text = serde_json::to_string(&cmd).unwrap();
        match parse_agent_command_text(&text) {
            AgentTextOutcome::Input(InputEvent::Key { c, d }) => {
                assert_eq!(c, 100);
                assert!(!d);
            }
            other => panic!("Expected Input, got {other:?}"),
        }
    }

    #[test]
    fn parse_agent_command_text_invalid() {
        match parse_agent_command_text("garbage") {
            AgentTextOutcome::Invalid(_) => {}
            other => panic!("Expected Invalid, got {other:?}"),
        }
    }

    #[test]
    fn parse_agent_command_text_empty() {
        match parse_agent_command_text("") {
            AgentTextOutcome::Invalid(_) => {}
            other => panic!("Expected Invalid, got {other:?}"),
        }
    }

    // --- should_log_initial_video_frame ---

    #[test]
    fn initial_frame_logs_first_three() {
        assert!(should_log_initial_video_frame(1));
        assert!(should_log_initial_video_frame(2));
        assert!(should_log_initial_video_frame(3));
    }

    #[test]
    fn initial_frame_skips_from_fourth_onwards() {
        assert!(!should_log_initial_video_frame(4));
        assert!(!should_log_initial_video_frame(100));
        assert!(!should_log_initial_video_frame(u64::MAX));
    }

    #[test]
    fn initial_frame_logs_at_zero_count() {
        // Defensive — the production code never asks at count=0, but
        // the helper must not panic.
        assert!(should_log_initial_video_frame(0));
    }

    // --- pong_timeout_elapsed ---

    #[test]
    fn pong_timeout_elapsed_true_when_exceeded() {
        assert!(pong_timeout_elapsed(31_000, 30_000));
        assert!(pong_timeout_elapsed(u64::MAX, 30_000));
    }

    #[test]
    fn pong_timeout_elapsed_false_when_under() {
        assert!(!pong_timeout_elapsed(29_000, 30_000));
        assert!(!pong_timeout_elapsed(0, 30_000));
    }

    #[test]
    fn pong_timeout_elapsed_strict_greater_than() {
        // Exactly at timeout is NOT yet "elapsed".
        assert!(!pong_timeout_elapsed(30_000, 30_000));
    }

    // --- browser_replaced_message ---

    #[test]
    fn browser_replaced_message_static() {
        let m = browser_replaced_message();
        assert!(m.contains("Browser replaced"));
        assert!(m.contains("new connection"));
    }

    // --- invalid_browser_message_format ---

    #[test]
    fn invalid_browser_message_includes_error() {
        let m = invalid_browser_message_format("expected `,` at line 1");
        assert!(m.contains("Invalid message format"));
        assert!(m.contains("expected `,`"));
    }

    #[test]
    fn invalid_browser_message_handles_empty_error() {
        let m = invalid_browser_message_format("");
        assert!(m.starts_with("Invalid message format"));
    }
}
