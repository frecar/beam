use crate::filetransfer;
use crate::signaling::WsSender;

use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

/// File download: read file on blocking thread, stream chunks via WebSocket text.
pub(crate) async fn run_file_download_loop(
    download_request_rx: &mut mpsc::Receiver<String>,
    file_transfer: &Arc<Mutex<filetransfer::FileTransferManager>>,
    ws_tx: &WsSender,
) {
    while let Some(path) = download_request_rx.recv().await {
        info!(path, "File download request received");
        let ft = Arc::clone(file_transfer);
        let ws = ws_tx.clone();

        // Bounded channel provides backpressure: the blocking reader
        // pauses when 16 messages are buffered, avoiding unbounded
        // memory growth for large files.
        let (chunk_tx, mut chunk_rx) = mpsc::channel::<String>(16);

        // File I/O is blocking -- run on a blocking thread, streaming
        // chunks through the bounded channel instead of collecting all.
        tokio::task::spawn_blocking(move || {
            let send_fn = |msg: String| {
                let _ = chunk_tx.blocking_send(msg);
            };
            let mgr = ft.lock().unwrap_or_else(|e| e.into_inner());
            let _ = mgr.handle_download_request(&path, &send_fn);
            // chunk_tx drops here, closing the channel
        });

        // Stream messages to browser as they arrive from the reader
        while let Some(msg) = chunk_rx.recv().await {
            if let Err(e) = ws.send(Message::Text(msg.into())).await {
                warn!("Failed to send download message to browser: {e}");
                break;
            }
        }
    }
}
