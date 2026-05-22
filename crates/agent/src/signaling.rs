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
}
