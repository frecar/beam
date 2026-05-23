use crate::clipboard::ClipboardBridge;
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
            cb.get_text()
        };
        match text {
            Ok(Some(ref text)) if !text.is_empty() => {
                if let Some(msg) = build_clipboard_payload(text) {
                    if let Err(e) = ws_tx.send(Message::Text(msg.into())).await {
                        warn!("Failed to send clipboard to browser: {e}");
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
}
