use crate::CaptureCommand;
use crate::h264;
use crate::signaling::WsSender;

use beam_protocol::VideoFrameHeader;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

/// Write encoded video frames as WebSocket binary messages.
/// Each frame is prefixed with a 24-byte VideoFrameHeader.
pub(crate) async fn run_video_send_loop(
    encoded_rx: &mut mpsc::Receiver<Vec<u8>>,
    ws_tx: &WsSender,
    force_keyframe: &Arc<AtomicBool>,
    capture_cmd_tx: &std::sync::mpsc::Sender<CaptureCommand>,
    capture_width: &Arc<std::sync::atomic::AtomicU32>,
    capture_height: &Arc<std::sync::atomic::AtomicU32>,
) {
    let mut video_frame_count: u64 = 0;
    let mut waiting_for_idr = true; // Start waiting for first IDR
    let mut idr_wait_start = Instant::now();
    let mut idr_wait_attempts: u32 = 0;
    let mut encoder_reset_count: u32 = 0;
    const MAX_ENCODER_RESETS: u32 = 3;
    let capture_start = Instant::now();

    while let Some(data) = encoded_rx.recv().await {
        let is_idr = h264::h264_contains_idr(&data);

        // Gate on first IDR frame — browser decoder needs a keyframe to initialize
        if waiting_for_idr {
            if !is_idr {
                if idr_wait_start.elapsed() > Duration::from_millis(500) {
                    idr_wait_attempts += 1;
                    if idr_wait_attempts > 5 {
                        if encoder_reset_count < MAX_ENCODER_RESETS {
                            encoder_reset_count += 1;
                            warn!(
                                attempts = idr_wait_attempts,
                                reset = encoder_reset_count,
                                max_resets = MAX_ENCODER_RESETS,
                                "Failed to get IDR, resetting encoder pipeline"
                            );
                            let _ = capture_cmd_tx.send(CaptureCommand::ResetEncoder);
                            idr_wait_start = Instant::now();
                            idr_wait_attempts = 0;
                        } else {
                            error!(
                                resets = encoder_reset_count,
                                "Exhausted encoder resets, proceeding with P-frames"
                            );
                            waiting_for_idr = false;
                        }
                    } else {
                        info!(
                            attempt = idr_wait_attempts,
                            waited_ms = idr_wait_start.elapsed().as_millis() as u64,
                            "IDR wait timeout, forcing another keyframe"
                        );
                        force_keyframe.store(true, Ordering::Relaxed);
                        idr_wait_start = Instant::now();
                    }
                }
                if waiting_for_idr {
                    continue;
                }
            }
            info!(
                size = data.len(),
                waited_ms = idr_wait_start.elapsed().as_millis() as u64,
                "First IDR frame, starting video stream"
            );
            waiting_for_idr = false;
        }

        // Build binary frame: VideoFrameHeader + H.264 payload
        let width = capture_width.load(Ordering::Relaxed) as u16;
        let height = capture_height.load(Ordering::Relaxed) as u16;
        let timestamp_us = capture_start.elapsed().as_micros() as u64;
        let header =
            VideoFrameHeader::video(width, height, timestamp_us, data.len() as u32, is_idr);
        let frame_bytes = header.serialize_with_payload(&data);

        match ws_tx.try_send(Message::Binary(frame_bytes.into())) {
            Ok(()) => {
                video_frame_count += 1;
                if video_frame_count <= 5 {
                    info!(
                        size = data.len(),
                        is_idr,
                        frame = video_frame_count,
                        "Video frame sent via WebSocket"
                    );
                }
                if video_frame_count.is_multiple_of(300) {
                    info!(video_frame_count, "Video frames sent");
                }
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                debug!("Dropping video frame (WS outbox full, prioritizing latency)");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                info!("WS outbox closed, stopping video send");
                break;
            }
        }
    }
    info!("Video frame channel closed");
}

/// Write encoded audio frames as WebSocket binary messages.
/// Uses the same VideoFrameHeader format with the audio flag set.
pub(crate) async fn run_audio_send_loop(audio_rx: &mut mpsc::Receiver<Vec<u8>>, ws_tx: &WsSender) {
    let capture_start = Instant::now();
    let mut audio_frame_count: u64 = 0;
    while let Some(data) = audio_rx.recv().await {
        let timestamp_us = capture_start.elapsed().as_micros() as u64;
        let header = VideoFrameHeader::audio(timestamp_us, data.len() as u32);
        let frame_bytes = header.serialize_with_payload(&data);

        match ws_tx.try_send(Message::Binary(frame_bytes.into())) {
            Ok(()) => {
                audio_frame_count += 1;
                if audio_frame_count <= 3 {
                    info!(
                        size = data.len(),
                        frame = audio_frame_count,
                        "Audio frame sent via WebSocket"
                    );
                }
                if audio_frame_count.is_multiple_of(500) {
                    info!(audio_frame_count, "Audio frames sent");
                }
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                warn!("Dropping audio frame (WS outbox full)");
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                info!("WS outbox closed, stopping audio send");
                break;
            }
        }
    }
    info!("Audio frame channel closed");
}
