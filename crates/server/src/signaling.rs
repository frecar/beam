use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use beam_protocol::{AgentCommand, FRAME_MAGIC, InputEvent, SignalingMessage};
use bytes::Bytes;
use tokio::sync::{Notify, RwLock, broadcast};
use tokio::time::{Duration, Instant, interval};
use uuid::Uuid;

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
        let (video_frames, _) = broadcast::channel(16);
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
pub async fn handle_browser_ws(mut socket: WebSocket, session_id: Uuid, registry: ChannelRegistry) {
    tracing::info!(%session_id, "Browser WebSocket upgrade request");
    let channel = get_or_create_channel(&registry, session_id).await;

    // Kick any existing browser for this session
    tracing::info!(%session_id, "Kicking any existing browsers for this session");
    channel.browser_kick.notify_waiters();

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

    loop {
        tokio::select! {
            // Kicked by a newer browser connection
            _ = &mut kicked => {
                tracing::info!(%session_id, "Browser replaced by new connection");
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
                if last_pong.elapsed() > WS_PONG_TIMEOUT {
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
                        if socket.send(Message::Binary(frame.to_vec().into())).await.is_err() {
                            tracing::debug!(%session_id, "Browser WebSocket binary send failed");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!(%session_id, skipped = n, "Browser video consumer lagged, frames dropped");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            // Receive messages from browser and forward to agent
            Some(result) = socket.recv() => {
                match result {
                    Ok(Message::Text(text)) => {
                        // Try parsing as InputEvent first (most common)
                        match serde_json::from_str::<InputEvent>(&text) {
                            Ok(event) => {
                                let cmd = AgentCommand::Input(event);
                                if let Err(e) = channel.to_agent.send(cmd) {
                                    tracing::warn!(%session_id, "No agent listening for input: {e}");
                                }
                            }
                            Err(e) => {
                                tracing::warn!(%session_id, "Invalid browser message: {e}");
                                let err = SignalingMessage::Error {
                                    message: format!("Invalid message format: {e}"),
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
                if last_pong.elapsed() > WS_PONG_TIMEOUT {
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
                        if len >= 4 {
                            let magic = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
                            if magic == FRAME_MAGIC {
                                let receivers = channel.video_frames.receiver_count();
                                match channel.video_frames.send(Bytes::from(data.to_vec())) {
                                    Ok(n) => {
                                        tracing::trace!(%session_id, len, receivers, sent_to = n, "Relayed binary frame");
                                    }
                                    Err(e) => {
                                        tracing::debug!(%session_id, len, receivers, "No browser listening for video: {e}");
                                    }
                                }
                            } else {
                                tracing::warn!(%session_id, "Agent sent binary with bad magic: 0x{magic:08x}");
                            }
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
