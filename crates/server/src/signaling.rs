use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use beam_protocol::{AgentCommand, SignalingMessage};
use tokio::sync::{broadcast, Notify, RwLock};
use uuid::Uuid;

/// Per-session signaling channel with separate paths for browser→agent and agent→browser.
/// This prevents echo (messages reflecting back to the sender).
pub struct SignalingChannel {
    /// Messages from browser clients, consumed by the agent
    pub to_agent: broadcast::Sender<SignalingMessage>,
    /// Messages from the agent, consumed by browser clients
    pub to_browser: broadcast::Sender<SignalingMessage>,
    /// Notified when a new browser connects, kicking the previous one.
    /// Only one browser WebSocket per session is supported at a time.
    pub browser_kick: Notify,
}

impl SignalingChannel {
    pub fn new() -> Self {
        let (to_agent, _) = broadcast::channel(64);
        let (to_browser, _) = broadcast::channel(64);
        Self {
            to_agent,
            to_browser,
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
/// Browser sends → to_agent channel (agent reads these).
/// Browser receives ← to_browser channel (agent writes these).
///
/// Only one browser per session at a time. Connecting a new browser
/// kicks the previous one with close code 4001 ("replaced").
pub async fn handle_browser_ws(mut socket: WebSocket, session_id: Uuid, registry: ChannelRegistry) {
    let channel = get_or_create_channel(&registry, session_id).await;

    // Kick any existing browser for this session
    channel.browser_kick.notify_waiters();

    let mut from_agent = channel.to_browser.subscribe();
    // Register our own kick listener AFTER kicking the old browser
    let kicked = channel.browser_kick.notified();
    tokio::pin!(kicked);

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
            // Forward agent messages to this browser client
            result = from_agent.recv() => {
                let msg = match result {
                    Ok(m) => m,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(%session_id, skipped = n, "Browser consumer lagged, messages dropped");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let json = match serde_json::to_string(&msg) {
                    Ok(j) => j,
                    Err(e) => {
                        tracing::error!("Failed to serialize signaling message: {e}");
                        continue;
                    }
                };
                if socket.send(Message::Text(json.into())).await.is_err() {
                    tracing::debug!(%session_id, "Browser WebSocket send failed");
                    break;
                }
            }
            // Receive messages from browser and forward to agent
            Some(result) = socket.recv() => {
                match result {
                    Ok(Message::Text(text)) => {
                        match serde_json::from_str::<SignalingMessage>(&text) {
                            Ok(msg) => {
                                tracing::debug!(%session_id, ?msg, "Browser → Agent");
                                if let Err(e) = channel.to_agent.send(msg) {
                                    tracing::warn!(%session_id, "No agent listening: {e}");
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
/// Agent sends → to_browser channel (browser reads these).
/// Agent receives ← to_agent channel wrapped in AgentCommand::Signal.
pub async fn handle_agent_ws(mut socket: WebSocket, session_id: Uuid, registry: ChannelRegistry) {
    let channel = get_or_create_channel(&registry, session_id).await;
    let mut from_browser = channel.to_agent.subscribe();

    tracing::info!(%session_id, "Agent WebSocket connected");

    loop {
        tokio::select! {
            // Forward browser messages to agent (wrapped in AgentCommand)
            result = from_browser.recv() => {
                let msg = match result {
                    Ok(m) => m,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(%session_id, skipped = n, "Agent consumer lagged, messages dropped");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                let cmd = AgentCommand::Signal(msg);
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
            // Receive messages from agent and forward to browser
            Some(result) = socket.recv() => {
                match result {
                    Ok(Message::Text(text)) => {
                        match serde_json::from_str::<SignalingMessage>(&text) {
                            Ok(msg) => {
                                tracing::debug!(%session_id, ?msg, "Agent → Browser");
                                if let Err(e) = channel.to_browser.send(msg) {
                                    tracing::warn!(%session_id, "No browser listening: {e}");
                                }
                            }
                            Err(e) => {
                                tracing::warn!(%session_id, "Invalid agent message: {e}");
                            }
                        }
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
