use crate::clipboard::ClipboardBridge;
use crate::peer::{self, SharedPeer};

use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Clipboard sync: after Ctrl+C/X, read X11 clipboard and send to browser.
pub(crate) async fn run_clipboard_sync(
    clipboard_read_rx: &mut mpsc::Receiver<()>,
    clipboard: &Arc<Mutex<ClipboardBridge>>,
    shared_peer: &SharedPeer,
) {
    while let Some(()) = clipboard_read_rx.recv().await {
        // Brief delay so the X11 app has time to write to the clipboard
        tokio::time::sleep(Duration::from_millis(100)).await;
        let text = {
            let cb = clipboard.lock().unwrap_or_else(|e| e.into_inner());
            cb.get_text()
        };
        match text {
            Ok(Some(ref text)) if !text.is_empty() => {
                const MAX_CLIPBOARD_BYTES: usize = 1_048_576;
                if text.len() <= MAX_CLIPBOARD_BYTES {
                    let msg = serde_json::json!({ "t": "c", "text": text }).to_string();
                    let current_peer = peer::snapshot(shared_peer).await;
                    if let Err(e) = current_peer.send_data_channel_message(&msg).await {
                        warn!("Failed to send clipboard to browser: {e:#}");
                    } else {
                        info!(len = text.len(), "Clipboard text sent to browser");
                    }
                }
            }
            Ok(_) => {
                info!("Clipboard read returned empty/none");
            }
            Err(e) => {
                warn!("Failed to read clipboard: {e:#}");
            }
        }
    }
}
