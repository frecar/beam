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
    video_needs_keyframe: &Arc<AtomicBool>,
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

        // On browser reconnect / tab foreground, the input callback sets
        // video_needs_keyframe. Drop P-frames until the forced IDR arrives
        // so the reconnecting browser's decoder gets a clean keyframe as
        // its first frame. This flag is separate from force_keyframe (which
        // the capture thread clears immediately via swap), so there's no
        // race between the capture thread and this async loop.
        if video_needs_keyframe.load(Ordering::Relaxed) && !waiting_for_idr {
            if is_idr {
                video_needs_keyframe.store(false, Ordering::Relaxed);
                info!(size = data.len(), "Reconnect IDR received, resuming stream");
            } else {
                continue; // Drop P-frame while waiting for forced IDR
            }
        }

        // Gate on first IDR frame — browser decoder needs a keyframe to initialize.
        // Also require minimum IDR size: during desktop startup (blank screen),
        // nvh264enc produces tiny IDRs (~500 bytes) that Chrome's hardware
        // VideoDecoder rejects with EncodingError. Wait for real desktop content
        // which produces IDRs of at least 1KB.
        const MIN_IDR_SIZE: usize = 1024;
        if waiting_for_idr {
            if !is_idr || data.len() < MIN_IDR_SIZE {
                if is_idr && data.len() < MIN_IDR_SIZE {
                    debug!(
                        size = data.len(),
                        min = MIN_IDR_SIZE,
                        "Skipping undersized IDR (desktop likely still blank)"
                    );
                    // Force another keyframe soon
                    force_keyframe.store(true, Ordering::Relaxed);
                }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    /// Build a fake H.264 IDR frame (NAL type 5 with 4-byte start code).
    fn fake_idr(size: usize) -> Vec<u8> {
        let mut data = vec![0x00, 0x00, 0x00, 0x01, 0x65]; // start code + IDR NAL
        data.resize(size, 0xAB); // pad to requested size
        data
    }

    /// Build a fake H.264 P-frame (NAL type 1 with 4-byte start code).
    fn fake_p_frame(size: usize) -> Vec<u8> {
        let mut data = vec![0x00, 0x00, 0x00, 0x01, 0x61]; // start code + non-IDR NAL
        data.resize(size, 0xCD);
        data
    }

    /// Helper to run the video send loop with controlled inputs and collect output.
    async fn run_loop_collect(
        frames: Vec<Vec<u8>>,
        video_needs_keyframe: bool,
    ) -> Vec<(usize, bool)> {
        let (encoded_tx, mut encoded_rx) = mpsc::channel::<Vec<u8>>(32);
        let (ws_tx, mut ws_rx) = mpsc::channel::<Message>(32);
        let force_kf = Arc::new(AtomicBool::new(false));
        let video_nk = Arc::new(AtomicBool::new(video_needs_keyframe));
        let (cmd_tx, _cmd_rx) = std::sync::mpsc::channel::<CaptureCommand>();
        let width = Arc::new(AtomicU32::new(1920));
        let height = Arc::new(AtomicU32::new(1080));

        // Send an initial IDR to get past the waiting_for_idr gate
        encoded_tx.send(fake_idr(2048)).await.unwrap();

        // Send the test frames
        for frame in &frames {
            encoded_tx.send(frame.clone()).await.unwrap();
        }
        drop(encoded_tx); // Close channel to end the loop

        // Run the send loop
        let handle = tokio::spawn(async move {
            run_video_send_loop(
                &mut encoded_rx,
                &ws_tx,
                &force_kf,
                &video_nk,
                &cmd_tx,
                &width,
                &height,
            )
            .await;
        });

        // Collect sent frames
        let mut results = Vec::new();
        while let Some(msg) = ws_rx.recv().await {
            if let Message::Binary(data) = msg {
                if let Ok(header) = beam_protocol::VideoFrameHeader::deserialize(&data) {
                    if !header.is_audio() {
                        results.push((header.payload_length as usize, header.is_keyframe()));
                    }
                }
            }
        }
        handle.await.unwrap();
        results
    }

    #[tokio::test]
    async fn video_loop_sends_idr_then_p_frames() {
        // Normal flow: IDR followed by P-frames, no gating.
        let frames = vec![fake_p_frame(500), fake_p_frame(500), fake_p_frame(500)];

        let results = run_loop_collect(frames, false).await;

        // Should get: initial IDR + 3 P-frames = 4 frames
        assert_eq!(results.len(), 4, "Should send initial IDR + 3 P-frames");
        assert!(results[0].1, "First frame should be keyframe");
        assert!(!results[1].1, "Second frame should be P-frame");
    }

    #[tokio::test]
    async fn video_loop_gates_p_frames_when_needs_keyframe() {
        // Reconnect scenario: video_needs_keyframe is true, P-frames should
        // be dropped until an IDR arrives.
        let frames = vec![
            fake_p_frame(500), // should be dropped
            fake_p_frame(500), // should be dropped
            fake_idr(2048),    // should pass through (reconnect IDR)
            fake_p_frame(500), // should pass through (after IDR)
        ];

        let results = run_loop_collect(frames, true).await;

        // Should get: initial IDR, then reconnect IDR + 1 P-frame (2 P-frames dropped)
        assert_eq!(
            results.len(),
            3,
            "Should send initial IDR + reconnect IDR + 1 P-frame"
        );
        assert!(results[0].1, "First frame should be initial keyframe");
        assert!(results[1].1, "Second frame should be reconnect keyframe");
        assert!(!results[2].1, "Third frame should be P-frame");
    }

    #[tokio::test]
    async fn video_loop_idr_gate_clears_after_idr() {
        // After the reconnect IDR arrives, subsequent P-frames should flow normally.
        let frames = vec![
            fake_p_frame(500), // dropped (gated)
            fake_idr(2048),    // passes (clears gate)
            fake_p_frame(500), // passes
            fake_p_frame(500), // passes
            fake_p_frame(500), // passes
        ];

        let results = run_loop_collect(frames, true).await;

        // initial IDR + reconnect IDR + 3 P-frames = 5
        assert_eq!(results.len(), 5, "Gate should clear after IDR");
    }

    #[tokio::test]
    async fn video_loop_no_gate_when_flag_not_set() {
        // When video_needs_keyframe is false, all frames pass through.
        let frames = vec![
            fake_p_frame(500),
            fake_p_frame(500),
            fake_idr(2048),
            fake_p_frame(500),
        ];

        let results = run_loop_collect(frames, false).await;

        // initial IDR + all 4 frames = 5
        assert_eq!(results.len(), 5, "All frames should pass when not gated");
    }

    #[tokio::test]
    async fn video_loop_skips_undersized_initial_idr() {
        // During startup, tiny IDRs (< 1024 bytes) from blank desktop
        // are skipped. Only a real IDR (>= 1024 bytes) starts the stream.
        let (encoded_tx, mut encoded_rx) = mpsc::channel::<Vec<u8>>(32);
        let (ws_tx, mut ws_rx) = mpsc::channel::<Message>(32);
        let force_kf = Arc::new(AtomicBool::new(false));
        let video_nk = Arc::new(AtomicBool::new(false));
        let (cmd_tx, _cmd_rx) = std::sync::mpsc::channel::<CaptureCommand>();
        let width = Arc::new(AtomicU32::new(1920));
        let height = Arc::new(AtomicU32::new(1080));

        // Tiny IDR (should be skipped)
        encoded_tx.send(fake_idr(500)).await.unwrap();
        // Real IDR (should start stream)
        encoded_tx.send(fake_idr(2048)).await.unwrap();
        // P-frame after
        encoded_tx.send(fake_p_frame(500)).await.unwrap();
        drop(encoded_tx);

        tokio::spawn(async move {
            run_video_send_loop(
                &mut encoded_rx,
                &ws_tx,
                &force_kf,
                &video_nk,
                &cmd_tx,
                &width,
                &height,
            )
            .await;
        });

        let mut count = 0;
        while let Some(_msg) = ws_rx.recv().await {
            count += 1;
        }
        // Tiny IDR skipped, real IDR + P-frame sent = 2 frames
        assert_eq!(count, 2, "Undersized IDR should be skipped");
    }
}
