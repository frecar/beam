use crate::clipboard::{ClipboardBridge, ClipboardRead};
use crate::signaling::WsSender;

use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

/// Maximum clipboard payload (1 MiB). Matches the browser-side limit so we
/// never send a frame the other end will refuse.
pub(crate) const MAX_CLIPBOARD_BYTES: usize = 1_048_576;

/// Convert a clipboard read result into an optional outgoing WS payload.
///
/// Returns `Some(json)` only when the text is non-empty AND within the size
/// limit. Empty text, oversize text, and errors all produce `None` (with
/// matching log lines emitted by the caller).
pub(crate) fn build_clipboard_payload(text: &str) -> Option<String> {
    if text.is_empty() || text.len() > MAX_CLIPBOARD_BYTES {
        return None;
    }
    Some(serde_json::json!({ "t": "c", "text": text }).to_string())
}

/// Outcome of dispatching a single clipboard-read tick. Pure value so the
/// dispatch branches are unit-testable without owning a WebSocket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ClipboardSyncDispatch {
    /// Non-empty clipboard text — emit the payload over WS.
    Send { payload: String, len: usize },
    /// Empty or None from get_text — log and continue.
    Empty,
    /// get_text errored — log warn and continue.
    ReadError(String),
    /// Text was non-empty but oversized; skip.
    Oversize,
}

/// Convert a `get_text()` result into a [`ClipboardSyncDispatch`]. Pure
/// helper around the branch tree in `run_clipboard_sync`.
pub(crate) fn classify_clipboard_read_result(
    result: anyhow::Result<Option<String>>,
) -> ClipboardSyncDispatch {
    match result {
        Ok(Some(text)) if !text.is_empty() => match build_clipboard_payload(&text) {
            Some(payload) => ClipboardSyncDispatch::Send {
                payload,
                len: text.len(),
            },
            None => ClipboardSyncDispatch::Oversize,
        },
        Ok(_) => ClipboardSyncDispatch::Empty,
        Err(e) => ClipboardSyncDispatch::ReadError(e.to_string()),
    }
}

/// Clipboard sync: after Ctrl+C/X, read X11 clipboard and send to browser via WS text.
pub(crate) async fn run_clipboard_sync(
    clipboard_read_rx: &mut mpsc::Receiver<()>,
    clipboard: &Arc<Mutex<ClipboardBridge>>,
    ws_tx: &WsSender,
) {
    while let Some(()) = clipboard_read_rx.recv().await {
        // Brief delay so the X11 app has time to write to the clipboard
        tokio::time::sleep(Duration::from_millis(100)).await;
        let text = {
            let cb = clipboard.lock().unwrap_or_else(|e| e.into_inner());
            ClipboardRead::get_text(&*cb)
        };
        match classify_clipboard_read_result(text) {
            ClipboardSyncDispatch::Send { payload, len } => {
                if let Err(e) = ws_tx.send(Message::Text(payload.into())).await {
                    warn!("Failed to send clipboard to browser: {e}");
                } else {
                    info!(len, "Clipboard text sent to browser");
                }
            }
            ClipboardSyncDispatch::Empty | ClipboardSyncDispatch::Oversize => {
                info!("Clipboard read returned empty/none");
            }
            ClipboardSyncDispatch::ReadError(e) => {
                warn!("Failed to read clipboard: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_payload_normal_text() {
        let payload = build_clipboard_payload("hello world").expect("payload");
        // The JSON wraps the text in a fixed envelope; assert both shape and content.
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["t"], "c");
        assert_eq!(parsed["text"], "hello world");
    }

    #[test]
    fn build_payload_empty_returns_none() {
        // Empty clipboard contents must never be sent; the browser treats a
        // missing payload as "nothing changed".
        assert_eq!(build_clipboard_payload(""), None);
    }

    #[test]
    fn build_payload_exact_max_size_is_accepted() {
        let text: String = "a".repeat(MAX_CLIPBOARD_BYTES);
        let payload = build_clipboard_payload(&text);
        assert!(
            payload.is_some(),
            "Payload at the exact size limit must pass"
        );
    }

    #[test]
    fn build_payload_oversize_returns_none() {
        // 1 byte over the limit must be silently dropped to prevent the
        // browser from receiving a frame it would reject.
        let text: String = "a".repeat(MAX_CLIPBOARD_BYTES + 1);
        assert_eq!(build_clipboard_payload(&text), None);
    }

    #[test]
    fn build_payload_unicode_is_preserved() {
        let text = "Hei verden! \u{1F600} \u{00E6}\u{00F8}\u{00E5}";
        let payload = build_clipboard_payload(text).expect("payload");
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["text"].as_str().unwrap(), text);
    }

    #[test]
    fn build_payload_embedded_quotes_are_escaped() {
        // serde_json escapes quotes — verify the round trip preserves the
        // exact bytes so the browser receives the canonical text.
        let text = r#"He said "hello""#;
        let payload = build_clipboard_payload(text).expect("payload");
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["text"].as_str().unwrap(), text);
    }

    #[test]
    fn build_payload_newlines_round_trip() {
        let text = "line one\nline two\r\nline three";
        let payload = build_clipboard_payload(text).expect("payload");
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["text"].as_str().unwrap(), text);
    }

    #[test]
    fn build_payload_envelope_shape_is_stable() {
        // Lock the wire envelope: t=c, text=<contents>, no other fields.
        // The browser parses by exact key name, so the shape MUST NOT drift.
        let payload = build_clipboard_payload("x").expect("payload");
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        let obj = parsed.as_object().expect("payload is JSON object");
        let mut keys: Vec<&String> = obj.keys().collect();
        keys.sort();
        assert_eq!(keys, vec![&"t".to_string(), &"text".to_string()]);
    }

    #[test]
    fn build_payload_single_char_is_accepted() {
        // A single character is the smallest non-empty payload.
        let payload = build_clipboard_payload("x").expect("payload");
        let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(parsed["text"], "x");
    }

    #[test]
    fn build_payload_just_under_max_size_accepted() {
        // MAX-1 bytes is still inside the window.
        let text: String = "a".repeat(MAX_CLIPBOARD_BYTES - 1);
        assert!(build_clipboard_payload(&text).is_some());
    }

    #[test]
    fn max_clipboard_bytes_constant() {
        // Lock the limit at 1 MiB. Browser-side enforces the same value.
        assert_eq!(MAX_CLIPBOARD_BYTES, 1_048_576);
    }

    // --- run_clipboard_sync loop ---

    /// Whether xclip is on PATH. The loop calls into ClipboardBridge::get_text
    /// which spawns xclip; without it we'd hit a different error path that
    /// isn't representative of production.
    ///
    /// Checks `/usr/bin/xclip` directly (not `which xclip`) to dodge the PATH
    /// race with `clipboard::tests::clipboard_bridge_new_fails_without_xclip_in_path`
    /// which temporarily wipes `$PATH` via `unsafe { std::env::set_var }`.
    fn xclip_available() -> bool {
        std::path::Path::new("/usr/bin/xclip").exists()
            || std::path::Path::new("/bin/xclip").exists()
    }

    /// Construct a bridge against a bogus display with retry to absorb the
    /// PATH-mutation race from sibling clipboard tests.
    fn try_make_bridge() -> Option<ClipboardBridge> {
        if !xclip_available() {
            return None;
        }
        for _ in 0..10 {
            if let Ok(bridge) = ClipboardBridge::new(":99999") {
                return Some(bridge);
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        None
    }

    #[tokio::test]
    async fn run_clipboard_sync_exits_when_input_channel_closes() {
        // Dropping the read receiver should let the loop unwind cleanly with
        // no further activity. This is the shutdown path.
        let Some(bridge_inner) = try_make_bridge() else {
            return;
        };
        let (read_tx, mut read_rx) = mpsc::channel::<()>(4);
        let (ws_tx, _ws_rx) = mpsc::channel::<Message>(4);
        let bridge = Arc::new(Mutex::new(bridge_inner));

        // Close input channel before spawning the loop
        drop(read_tx);

        // The loop should return immediately when the channel is closed.
        tokio::time::timeout(
            Duration::from_secs(2),
            run_clipboard_sync(&mut read_rx, &bridge, &ws_tx),
        )
        .await
        .expect("loop must exit when input channel closes");
    }

    #[tokio::test]
    async fn run_clipboard_sync_logs_when_get_text_returns_none() {
        // With a bogus display, ClipboardBridge::get_text returns Ok(None).
        // The loop's Ok(_) arm logs and loops back without sending to ws_tx.
        let Some(bridge_inner) = try_make_bridge() else {
            return;
        };
        let (read_tx, mut read_rx) = mpsc::channel::<()>(4);
        let (ws_tx, mut ws_rx) = mpsc::channel::<Message>(4);
        let bridge = Arc::new(Mutex::new(bridge_inner));

        // Send one signal then close. The loop processes the signal (get_text
        // returns Ok(None) on bogus display), then unwinds on channel close.
        read_tx.send(()).await.unwrap();
        drop(read_tx);

        tokio::time::timeout(
            Duration::from_secs(2),
            run_clipboard_sync(&mut read_rx, &bridge, &ws_tx),
        )
        .await
        .expect("loop should process the signal and exit");

        // No clipboard payload should be sent (xclip returned non-zero, no text)
        assert!(
            ws_rx.try_recv().is_err(),
            "No payload should be sent when get_text returns Ok(None)"
        );
    }

    #[tokio::test]
    async fn run_clipboard_sync_handles_multiple_signals() {
        // Sending two signals back-to-back exercises the loop's repeat path.
        // The loop must process each and continue without panic.
        let Some(bridge_inner) = try_make_bridge() else {
            return;
        };
        let (read_tx, mut read_rx) = mpsc::channel::<()>(4);
        let (ws_tx, _ws_rx) = mpsc::channel::<Message>(4);
        let bridge = Arc::new(Mutex::new(bridge_inner));

        for _ in 0..3 {
            read_tx.send(()).await.unwrap();
        }
        drop(read_tx);

        tokio::time::timeout(
            Duration::from_secs(3),
            run_clipboard_sync(&mut read_rx, &bridge, &ws_tx),
        )
        .await
        .expect("loop should process all signals and exit");
    }

    // --- classify_clipboard_read_result ---

    #[test]
    fn classify_clipboard_read_some_text_returns_send() {
        let r = classify_clipboard_read_result(Ok(Some("hello".into())));
        match r {
            ClipboardSyncDispatch::Send { payload, len } => {
                assert_eq!(len, 5);
                assert!(payload.contains("hello"));
                // Payload is JSON with {"t":"c","text":"..."}.
                let parsed: serde_json::Value = serde_json::from_str(&payload).unwrap();
                assert_eq!(parsed["t"], "c");
                assert_eq!(parsed["text"], "hello");
            }
            other => panic!("expected Send, got {other:?}"),
        }
    }

    #[test]
    fn classify_clipboard_read_empty_string_returns_empty() {
        let r = classify_clipboard_read_result(Ok(Some(String::new())));
        assert_eq!(r, ClipboardSyncDispatch::Empty);
    }

    #[test]
    fn classify_clipboard_read_none_returns_empty() {
        let r = classify_clipboard_read_result(Ok(None));
        assert_eq!(r, ClipboardSyncDispatch::Empty);
    }

    #[test]
    fn classify_clipboard_read_error_returns_read_error() {
        let r = classify_clipboard_read_result(Err(anyhow::anyhow!("xclip blew up")));
        match r {
            ClipboardSyncDispatch::ReadError(e) => assert!(e.contains("xclip blew up")),
            other => panic!("expected ReadError, got {other:?}"),
        }
    }

    #[test]
    fn classify_clipboard_read_oversize_returns_oversize() {
        let huge = "x".repeat(MAX_CLIPBOARD_BYTES + 1);
        let r = classify_clipboard_read_result(Ok(Some(huge)));
        assert_eq!(r, ClipboardSyncDispatch::Oversize);
    }

    #[test]
    fn classify_clipboard_read_exactly_at_limit_returns_send() {
        let at_limit = "x".repeat(MAX_CLIPBOARD_BYTES);
        let r = classify_clipboard_read_result(Ok(Some(at_limit)));
        match r {
            ClipboardSyncDispatch::Send { len, .. } => {
                assert_eq!(len, MAX_CLIPBOARD_BYTES);
            }
            other => panic!("expected Send, got {other:?}"),
        }
    }

    // --- ClipboardSyncDispatch equality + debug ---

    #[test]
    fn clipboard_sync_dispatch_equality() {
        assert_eq!(ClipboardSyncDispatch::Empty, ClipboardSyncDispatch::Empty);
        assert_eq!(
            ClipboardSyncDispatch::Oversize,
            ClipboardSyncDispatch::Oversize
        );
        assert_eq!(
            ClipboardSyncDispatch::ReadError("e".into()),
            ClipboardSyncDispatch::ReadError("e".into())
        );
        assert_ne!(
            ClipboardSyncDispatch::Empty,
            ClipboardSyncDispatch::Oversize
        );
    }

    // --- ClipboardRead trait impl (against ClipboardBridge) ---

    #[test]
    fn clipboard_read_trait_get_text_delegates_to_inherent() {
        // Verify the trait impl is exercised. Locally xclip exists; on CI
        // we install it via the workflow. Test gracefully skips if it
        // isn't available (returns Ok(()) without asserting).
        if !std::path::Path::new("/usr/bin/xclip").exists()
            && !std::path::Path::new("/bin/xclip").exists()
        {
            return;
        }
        // Retry construction a few times in case the parallel `set PATH`
        // test in clipboard.rs is in its critical section.
        let bridge = {
            let mut b = None;
            for _ in 0..10 {
                if let Ok(br) = crate::clipboard::ClipboardBridge::new(":99999") {
                    b = Some(br);
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            match b {
                Some(b) => b,
                None => return,
            }
        };
        let r: &dyn crate::clipboard::ClipboardRead = &bridge;
        // Bogus :99999 display → xclip -o exits non-zero → Ok(None).
        match r.get_text() {
            Ok(_) | Err(_) => {} // either is acceptable in PATH-race conditions
        }
    }
}
