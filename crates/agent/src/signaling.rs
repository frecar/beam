use crate::CaptureCommand;
use crate::peer::{self, PeerConfig, SharedPeer};

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use beam_protocol::{AgentCommand, InputEvent, SignalingMessage};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// Shared context for signaling WebSocket connection.
pub(crate) struct SignalingCtx<'a> {
    pub server_url: &'a str,
    pub session_id: Uuid,
    pub agent_token: Option<&'a str>,
    pub tls_cert_path: Option<&'a str>,
    pub shared_peer: &'a SharedPeer,
    pub peer_config: &'a PeerConfig,
    pub signal_tx: &'a mpsc::Sender<SignalingMessage>,
    pub force_keyframe: Arc<AtomicBool>,
    pub input_callback: Arc<dyn Fn(InputEvent) + Send + Sync>,
    pub capture_cmd_tx: &'a std::sync::mpsc::Sender<CaptureCommand>,
}

pub(crate) async fn run_signaling(
    ctx: &SignalingCtx<'_>,
    signal_rx: &mut mpsc::Receiver<SignalingMessage>,
) {
    if ctx.server_url.is_empty() {
        info!("No server URL provided, waiting for signaling via stdin or other mechanism");
        while let Some(msg) = signal_rx.recv().await {
            debug!(?msg, "Outgoing signal (no server connected)");
        }
        return;
    }

    // Connect to WebSocket with exponential backoff retry
    let mut backoff = Duration::from_secs(2);
    let max_backoff = Duration::from_secs(60);
    loop {
        info!(url = ctx.server_url, "Connecting to signaling server");

        match connect_and_handle(ctx, signal_rx).await {
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
/// Falls back to system roots if no cert path is given.
fn build_tls_connector(tls_cert_path: Option<&str>) -> tokio_tungstenite::Connector {
    let mut root_store = rustls::RootCertStore::empty();

    // Load system roots as baseline
    for cert in rustls_native_certs::load_native_certs().expect("Could not load platform certs") {
        let _ = root_store.add(cert);
    }

    // If a pinned cert PEM is provided, add it to the root store
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
    signal_rx: &mut mpsc::Receiver<SignalingMessage>,
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
    ws_config.max_message_size = Some(65_536); // 64KB max, matching server-side limit
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

    // Track the last processed offer's ICE ufrag to deduplicate retried
    // offers from the browser. The browser's offer retry mechanism can
    // re-send the same SDP before the answer arrives, which previously
    // caused the agent to create multiple peers -- only the last one
    // surviving, with the browser holding ICE credentials for a dead peer.
    let mut last_offer_ufrag: Option<String> = None;

    loop {
        tokio::select! {
            // Incoming messages from server
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<AgentCommand>(&text) {
                            Ok(AgentCommand::Signal(signal)) => {
                                match signal {
                                    SignalingMessage::Offer { sdp, .. } => {
                                        // Deduplicate: extract ICE ufrag from the SDP and
                                        // skip if we already processed this exact offer.
                                        // Browser retries re-send the same SDP (same ICE
                                        // credentials). Processing it again would destroy
                                        // the peer we just created for the first copy.
                                        let offer_ufrag = sdp.lines()
                                            .find(|l| l.starts_with("a=ice-ufrag:"))
                                            .map(|l| l.trim_start_matches("a=ice-ufrag:").to_string());
                                        if let Some(ref ufrag) = offer_ufrag
                                            && last_offer_ufrag.as_deref() == Some(ufrag) {
                                            info!(ufrag, "Ignoring duplicate SDP Offer (same ICE ufrag)");
                                            continue;
                                        }
                                        last_offer_ufrag = offer_ufrag;

                                        // Create a brand-new peer for every new SDP Offer.
                                        // This gives us fresh DTLS, ICE, SCTP, and data channel
                                        // state -- the only reliable way to handle browser
                                        // reconnects (refresh, new tab, etc.).
                                        info!("Received SDP Offer -- creating new WebRTC peer");
                                        let old_peer = peer::snapshot(ctx.shared_peer).await;
                                        // Close old peer (non-blocking best-effort)
                                        let _ = old_peer.close().await;

                                        match peer::create_peer(
                                            ctx.peer_config, ctx.signal_tx, ctx.session_id,
                                            &ctx.force_keyframe, Arc::clone(&ctx.input_callback),
                                        ).await {
                                            Ok(new_peer) => {
                                                // Handle the offer on the new peer
                                                match new_peer.handle_offer(&sdp).await {
                                                    Ok(answer_sdp) => {
                                                        // Swap the peer atomically
                                                        *ctx.shared_peer.write().await = new_peer;
                                                        info!("New peer installed, sending SDP answer");
                                                        let reply = SignalingMessage::Answer {
                                                            sdp: answer_sdp,
                                                            session_id: ctx.session_id,
                                                        };
                                                        let text = serde_json::to_string(&reply)?;
                                                        ws_tx.send(Message::Text(text.into())).await?;
                                                        // Recreate encoder to guarantee IDR on first frame.
                                                        // ForceKeyUnit events are unreliable on nvh264enc
                                                        // after long P-frame runs with gop-size=MAX.
                                                        let _ = ctx.capture_cmd_tx.send(CaptureCommand::ResetEncoder);
                                                        ctx.force_keyframe.store(true, Ordering::Relaxed);
                                                    }
                                                    Err(e) => {
                                                        error!("Failed to handle offer on new peer: {e:#}");
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                error!("Failed to create new peer: {e:#}");
                                            }
                                        }
                                    }
                                    SignalingMessage::IceCandidate {
                                        candidate, sdp_mid, sdp_mline_index, ..
                                    } => {
                                        let current_peer = peer::snapshot(ctx.shared_peer).await;
                                        if let Err(e) = current_peer
                                            .add_ice_candidate(&candidate, sdp_mid.as_deref(), sdp_mline_index)
                                            .await
                                        {
                                            warn!("Failed to add ICE candidate: {e:#}");
                                        }
                                    }
                                    other => {
                                        debug!(?other, "Unhandled signal message");
                                    }
                                }
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
            // Outgoing signaling messages (ICE candidates from our side)
            Some(msg) = signal_rx.recv() => {
                let text = serde_json::to_string(&msg)?;
                ws_tx.send(Message::Text(text.into())).await?;
            }
        }
    }
}
