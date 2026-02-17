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
        info!("No server URL provided, sleeping forever");
        std::future::pending::<()>().await;
        return;
    }

    // Connect to WebSocket with exponential backoff retry
    let mut backoff = Duration::from_secs(2);
    let max_backoff = Duration::from_secs(60);
    loop {
        info!(url = ctx.server_url, "Connecting to signaling server");

        match connect_and_handle(ctx, ws_outbox_rx).await {
            Ok(()) => {
                info!("Signaling connection closed cleanly");
                break;
            }
            Err(e) => {
                warn!("Signaling connection error: {e:#}");
                info!("Reconnecting in {} seconds...", backoff.as_secs());
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
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

async fn connect_and_handle(
    ctx: &SignalingCtx<'_>,
    ws_outbox_rx: &mut mpsc::Receiver<tokio_tungstenite::tungstenite::Message>,
) -> anyhow::Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let url = match ctx.agent_token {
        Some(token) => format!(
            "{}/ws/agent/{}?token={}",
            ctx.server_url,
            ctx.session_id,
            urlencoding::encode(token)
        ),
        None => format!("{}/ws/agent/{}", ctx.server_url, ctx.session_id),
    };

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
            // Incoming messages from server
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<AgentCommand>(&text) {
                            Ok(AgentCommand::Input(event)) => {
                                (ctx.input_callback)(event);
                            }
                            Ok(AgentCommand::Shutdown) => {
                                info!("Received shutdown command");
                                return Ok(());
                            }
                            Err(e) => {
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
