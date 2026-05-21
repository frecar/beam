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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn unique_dir(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "beam-fts-test-{}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4(),
            label,
        ))
    }

    #[tokio::test]
    async fn loop_streams_chunks_then_exits_on_channel_close() {
        // Build a small file inside a fresh home dir, then drive the loop
        // through a single download request and verify chunks reach the
        // WebSocket sender. The loop must terminate when we drop the request
        // channel, so the test can observe completion.
        let home = unique_dir("home");
        std::fs::create_dir_all(&home).unwrap();
        let payload = b"hello-from-beam-ft-task";
        let file_path = home.join("sample.txt");
        let mut f = std::fs::File::create(&file_path).unwrap();
        f.write_all(payload).unwrap();
        drop(f);

        let mgr = Arc::new(Mutex::new(filetransfer::FileTransferManager::new(
            home.clone(),
        )));
        // Buffered channel large enough to hold every chunk the reader emits.
        let (ws_tx, mut ws_rx) = mpsc::channel::<Message>(64);
        let (req_tx, mut req_rx) = mpsc::channel::<String>(4);

        // Drive the loop on a task so we can race the request + drop.
        let loop_handle = tokio::spawn({
            let mgr = Arc::clone(&mgr);
            let ws = ws_tx.clone();
            async move {
                run_file_download_loop(&mut req_rx, &mgr, &ws).await;
            }
        });
        // Drop our copy so the loop sees the channel close after the request drains.
        drop(ws_tx);

        req_tx
            .send(file_path.to_str().unwrap().to_string())
            .await
            .expect("loop should be running");
        // Close the request channel so the outer loop exits.
        drop(req_tx);

        loop_handle.await.expect("loop task should finish cleanly");

        // Drain every message the loop forwarded. At least one chunk message
        // must have been produced for a non-empty file.
        let mut chunks = 0usize;
        while let Some(_msg) = ws_rx.recv().await {
            chunks += 1;
        }
        assert!(
            chunks > 0,
            "loop should forward at least one chunk for a small file",
        );

        let _ = std::fs::remove_dir_all(&home);
    }

    #[tokio::test]
    async fn loop_swallows_send_errors_and_continues() {
        // If the WebSocket sender is closed before the reader finishes,
        // the loop logs + breaks out of the inner stream and waits for the
        // next request. Dropping the request channel afterwards must still
        // let the outer loop exit cleanly.
        let home = unique_dir("home-closed-ws");
        std::fs::create_dir_all(&home).unwrap();
        let file_path = home.join("sample.txt");
        std::fs::write(&file_path, b"payload").unwrap();

        let mgr = Arc::new(Mutex::new(filetransfer::FileTransferManager::new(
            home.clone(),
        )));
        let (ws_tx, ws_rx) = mpsc::channel::<Message>(1);
        drop(ws_rx); // sender will fail immediately
        let (req_tx, mut req_rx) = mpsc::channel::<String>(4);

        let loop_handle = tokio::spawn({
            let mgr = Arc::clone(&mgr);
            let ws = ws_tx.clone();
            async move {
                run_file_download_loop(&mut req_rx, &mgr, &ws).await;
            }
        });
        drop(ws_tx);

        req_tx
            .send(file_path.to_str().unwrap().to_string())
            .await
            .expect("loop running");
        drop(req_tx);

        loop_handle
            .await
            .expect("loop must finish even with closed ws");

        let _ = std::fs::remove_dir_all(&home);
    }
}
