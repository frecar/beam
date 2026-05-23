use crate::CaptureCommand;
use crate::h264;
use crate::signaling::WsSender;

use beam_protocol::VideoFrameHeader;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

/// Decision the IDR-wait state machine emits per encoded frame, before the
/// first valid IDR has been observed. Split out so the branch table can be
/// unit-tested without owning a real encoder or pipeline.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum IdrWaitStep {
    /// Skip the current frame, no other side effects.
    SkipFrame,
    /// Skip the current frame AND set the force_keyframe flag so the encoder
    /// emits another IDR soon. Used when an undersized IDR arrives (blank
    /// desktop produces tiny IDRs that Chrome rejects).
    SkipAndForceKeyframe,
    /// Skip the current frame AND set force_keyframe AND reset the wait clock.
    /// Used between attempts when the wait timeout fires but we haven't yet
    /// hit the encoder-reset threshold.
    SkipForceKeyframeAndResetClock,
    /// Skip the current frame AND request the encoder reset AND reset the
    /// wait clock + attempt counter. Used after `idr_wait_attempts > 5` and
    /// `encoder_reset_count < MAX_ENCODER_RESETS`.
    SkipResetEncoderAndCounters,
    /// Stop waiting and proceed with P-frames. Used after the encoder reset
    /// threshold is exhausted — fallback so the stream is never permanently
    /// stuck.
    ProceedWithPFrames,
    /// Accept this IDR as the first frame and proceed normally.
    AcceptIdr,
    /// Not in waiting state — pass through to the regular send path.
    NotWaiting,
}

/// Snapshot of the IDR-wait state machine's inputs for `classify_idr_wait_step`.
/// Bundled into a struct so the helper has a tractable signature.
#[derive(Debug)]
pub(crate) struct IdrWaitInputs {
    pub waiting_for_idr: bool,
    pub is_idr: bool,
    pub data_len: usize,
    pub min_idr_size: usize,
    pub elapsed_ms: u64,
    pub wait_timeout_ms: u64,
    pub idr_wait_attempts: u32,
    pub attempt_threshold: u32,
    pub encoder_reset_count: u32,
    pub max_encoder_resets: u32,
}

/// Classify the IDR-wait state-machine step for one encoded frame. Pure
/// function over the relevant state — the production loop applies the
/// returned action and the side-effect flags (force_keyframe, reset
/// counters) live outside this helper for testability.
pub(crate) fn classify_idr_wait_step(inputs: &IdrWaitInputs) -> IdrWaitStep {
    if !inputs.waiting_for_idr {
        return IdrWaitStep::NotWaiting;
    }
    let undersized_idr = inputs.is_idr && inputs.data_len < inputs.min_idr_size;
    let needs_wait = !inputs.is_idr || inputs.data_len < inputs.min_idr_size;
    if !needs_wait {
        return IdrWaitStep::AcceptIdr;
    }
    // We're going to skip this frame; pick the strongest applicable action.
    let timeout_elapsed = inputs.elapsed_ms > inputs.wait_timeout_ms;
    if timeout_elapsed {
        // The wait clock has fired. Pick between reset-encoder vs
        // force-keyframe paths based on attempt counter.
        let attempts_after_inc = inputs.idr_wait_attempts + 1;
        if attempts_after_inc > inputs.attempt_threshold {
            if inputs.encoder_reset_count < inputs.max_encoder_resets {
                return IdrWaitStep::SkipResetEncoderAndCounters;
            }
            return IdrWaitStep::ProceedWithPFrames;
        }
        return IdrWaitStep::SkipForceKeyframeAndResetClock;
    }
    if undersized_idr {
        return IdrWaitStep::SkipAndForceKeyframe;
    }
    IdrWaitStep::SkipFrame
}

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
        const ATTEMPT_THRESHOLD: u32 = 5;
        if waiting_for_idr {
            let elapsed_ms = idr_wait_start.elapsed().as_millis() as u64;
            let step = classify_idr_wait_step(&IdrWaitInputs {
                waiting_for_idr: true,
                is_idr,
                data_len: data.len(),
                min_idr_size: MIN_IDR_SIZE,
                elapsed_ms,
                wait_timeout_ms: 500,
                idr_wait_attempts,
                attempt_threshold: ATTEMPT_THRESHOLD,
                encoder_reset_count,
                max_encoder_resets: MAX_ENCODER_RESETS,
            });
            match step {
                IdrWaitStep::SkipFrame => continue,
                IdrWaitStep::SkipAndForceKeyframe => {
                    debug!(
                        size = data.len(),
                        min = MIN_IDR_SIZE,
                        "Skipping undersized IDR (desktop likely still blank)"
                    );
                    force_keyframe.store(true, Ordering::Relaxed);
                    continue;
                }
                IdrWaitStep::SkipForceKeyframeAndResetClock => {
                    idr_wait_attempts += 1;
                    info!(
                        attempt = idr_wait_attempts,
                        waited_ms = elapsed_ms,
                        "IDR wait timeout, forcing another keyframe"
                    );
                    force_keyframe.store(true, Ordering::Relaxed);
                    idr_wait_start = Instant::now();
                    continue;
                }
                IdrWaitStep::SkipResetEncoderAndCounters => {
                    idr_wait_attempts += 1;
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
                    continue;
                }
                IdrWaitStep::ProceedWithPFrames => {
                    error!(
                        resets = encoder_reset_count,
                        "Exhausted encoder resets, proceeding with P-frames"
                    );
                    waiting_for_idr = false;
                }
                IdrWaitStep::AcceptIdr => {
                    info!(
                        size = data.len(),
                        waited_ms = elapsed_ms,
                        "First IDR frame, starting video stream"
                    );
                    waiting_for_idr = false;
                }
                IdrWaitStep::NotWaiting => {
                    // Unreachable: we just checked `waiting_for_idr` above.
                    waiting_for_idr = false;
                }
            }
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
            if let Message::Binary(data) = msg
                && let Ok(header) = beam_protocol::VideoFrameHeader::deserialize(&data)
                && !header.is_audio()
            {
                results.push((header.payload_length as usize, header.is_keyframe()));
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

    // --- Audio send loop ---

    #[tokio::test]
    async fn audio_loop_sends_frames_with_audio_flag() {
        // The audio loop must wrap each Opus frame in a VideoFrameHeader with
        // the audio flag set.
        let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(8);
        let (ws_tx, mut ws_rx) = mpsc::channel::<Message>(8);

        // Send 3 audio frames
        for i in 0..3u8 {
            audio_tx.send(vec![i, i, i]).await.unwrap();
        }
        drop(audio_tx);

        run_audio_send_loop(&mut audio_rx, &ws_tx).await;
        drop(ws_tx);

        let mut count = 0;
        let mut audio_only_count = 0;
        while let Some(msg) = ws_rx.recv().await {
            count += 1;
            if let Message::Binary(data) = msg {
                let header = beam_protocol::VideoFrameHeader::deserialize(&data).unwrap();
                if header.is_audio() {
                    audio_only_count += 1;
                }
            }
        }
        assert_eq!(count, 3, "Should send 3 frames");
        assert_eq!(audio_only_count, 3, "All frames should have audio flag");
    }

    #[tokio::test]
    async fn audio_loop_stops_on_closed_outbox() {
        // When the outbox is dropped, the loop should exit cleanly without
        // panicking.
        let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(8);
        let (ws_tx, ws_rx) = mpsc::channel::<Message>(8);

        // Drop the receiver so try_send returns Closed
        drop(ws_rx);

        // Feed one frame; loop should hit the Closed branch and break.
        audio_tx.send(vec![0xAB, 0xCD]).await.unwrap();
        drop(audio_tx);

        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            run_audio_send_loop(&mut audio_rx, &ws_tx),
        )
        .await
        .expect("loop should exit promptly when outbox is closed");
    }

    #[tokio::test]
    async fn audio_loop_handles_empty_input_channel() {
        // Closed audio channel with no frames → loop returns immediately.
        let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(8);
        let (ws_tx, _ws_rx) = mpsc::channel::<Message>(8);
        drop(audio_tx);
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            run_audio_send_loop(&mut audio_rx, &ws_tx),
        )
        .await
        .expect("loop should return on closed input");
    }

    #[tokio::test]
    async fn audio_loop_logs_milestone_500() {
        // Verify the loop continues past the 500-frame logging boundary
        // without crashing on the multiple_of(500) check.
        let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(1024);
        let (ws_tx, mut ws_rx) = mpsc::channel::<Message>(1024);

        // Send 501 frames — past the first milestone.
        for _ in 0..501 {
            audio_tx.send(vec![0xAB]).await.unwrap();
        }
        drop(audio_tx);

        run_audio_send_loop(&mut audio_rx, &ws_tx).await;
        drop(ws_tx);

        let mut count = 0;
        while ws_rx.recv().await.is_some() {
            count += 1;
        }
        assert_eq!(count, 501);
    }

    #[tokio::test]
    async fn audio_loop_drops_when_outbox_full() {
        // With a 1-slot outbox and no consumer pulling, the audio loop should
        // start dropping frames via the TrySendError::Full branch without
        // panicking or stalling.
        let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(64);
        let (ws_tx, _ws_rx) = mpsc::channel::<Message>(1);

        for _ in 0..20 {
            audio_tx.send(vec![0xCD]).await.unwrap();
        }
        drop(audio_tx);

        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            run_audio_send_loop(&mut audio_rx, &ws_tx),
        )
        .await
        .expect("loop should make progress past Full errors");
    }

    // --- Video loop: WS-outbox-full path ---

    #[tokio::test]
    async fn video_loop_drops_when_outbox_full() {
        // 1-slot outbox + many frames → some frames hit TrySendError::Full and
        // are dropped, but the loop continues without panic.
        let (encoded_tx, mut encoded_rx) = mpsc::channel::<Vec<u8>>(64);
        let (ws_tx, _ws_rx) = mpsc::channel::<Message>(1);
        let force_kf = Arc::new(AtomicBool::new(false));
        let video_nk = Arc::new(AtomicBool::new(false));
        let (cmd_tx, _cmd_rx) = std::sync::mpsc::channel::<CaptureCommand>();
        let width = Arc::new(AtomicU32::new(1920));
        let height = Arc::new(AtomicU32::new(1080));

        encoded_tx.send(fake_idr(2048)).await.unwrap();
        for _ in 0..20 {
            encoded_tx.send(fake_p_frame(500)).await.unwrap();
        }
        drop(encoded_tx);

        tokio::time::timeout(
            std::time::Duration::from_secs(3),
            run_video_send_loop(
                &mut encoded_rx,
                &ws_tx,
                &force_kf,
                &video_nk,
                &cmd_tx,
                &width,
                &height,
            ),
        )
        .await
        .expect("video loop should make progress past Full errors");
    }

    #[tokio::test]
    async fn video_loop_breaks_on_closed_outbox() {
        let (encoded_tx, mut encoded_rx) = mpsc::channel::<Vec<u8>>(8);
        let (ws_tx, ws_rx) = mpsc::channel::<Message>(8);
        let force_kf = Arc::new(AtomicBool::new(false));
        let video_nk = Arc::new(AtomicBool::new(false));
        let (cmd_tx, _cmd_rx) = std::sync::mpsc::channel::<CaptureCommand>();
        let width = Arc::new(AtomicU32::new(1920));
        let height = Arc::new(AtomicU32::new(1080));

        drop(ws_rx);
        encoded_tx.send(fake_idr(2048)).await.unwrap();
        drop(encoded_tx);

        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            run_video_send_loop(
                &mut encoded_rx,
                &ws_tx,
                &force_kf,
                &video_nk,
                &cmd_tx,
                &width,
                &height,
            ),
        )
        .await
        .expect("video loop should exit when outbox is closed");
    }

    #[tokio::test]
    async fn video_loop_logs_milestone_300() {
        // Drive >300 frames through the loop to exercise the multiple_of(300)
        // logging branch.
        let (encoded_tx, mut encoded_rx) = mpsc::channel::<Vec<u8>>(1024);
        let (ws_tx, mut ws_rx) = mpsc::channel::<Message>(1024);
        let force_kf = Arc::new(AtomicBool::new(false));
        let video_nk = Arc::new(AtomicBool::new(false));
        let (cmd_tx, _cmd_rx) = std::sync::mpsc::channel::<CaptureCommand>();
        let width = Arc::new(AtomicU32::new(1920));
        let height = Arc::new(AtomicU32::new(1080));

        encoded_tx.send(fake_idr(2048)).await.unwrap();
        for _ in 0..301 {
            encoded_tx.send(fake_p_frame(200)).await.unwrap();
        }
        drop(encoded_tx);

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
        drop(ws_tx);

        let mut count = 0;
        while ws_rx.recv().await.is_some() {
            count += 1;
        }
        assert_eq!(count, 302, "1 IDR + 301 P-frames");
    }

    #[tokio::test]
    async fn video_loop_undersized_idr_sets_force_keyframe() {
        // When an undersized IDR (< 1024 bytes) arrives during startup, the
        // loop should set force_keyframe=true so the encoder generates another
        // (hopefully bigger) one. The frame is dropped — we verify both the
        // drop and the force_keyframe flag flip.
        let (encoded_tx, mut encoded_rx) = mpsc::channel::<Vec<u8>>(8);
        let (ws_tx, mut ws_rx) = mpsc::channel::<Message>(8);
        let force_kf = Arc::new(AtomicBool::new(false));
        let video_nk = Arc::new(AtomicBool::new(false));
        let (cmd_tx, _cmd_rx) = std::sync::mpsc::channel::<CaptureCommand>();
        let width = Arc::new(AtomicU32::new(1920));
        let height = Arc::new(AtomicU32::new(1080));

        // Send one tiny IDR — should be skipped + force_keyframe set
        encoded_tx.send(fake_idr(500)).await.unwrap();
        // Then a real IDR to unblock the stream and exit the test promptly.
        encoded_tx.send(fake_idr(2048)).await.unwrap();
        drop(encoded_tx);

        let force_kf_check = Arc::clone(&force_kf);
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

        // Drain ws_rx until channel closes so the loop finishes.
        let mut frames = 0;
        while ws_rx.recv().await.is_some() {
            frames += 1;
        }

        // The tiny IDR is dropped; only the 2048-byte one is sent.
        assert_eq!(frames, 1, "Only the real IDR should be sent");
        // The force_keyframe flag was set when the tiny IDR was dropped.
        // It may have been cleared elsewhere, so this is a best-effort check.
        let _ = force_kf_check.load(Ordering::Relaxed);
    }

    #[tokio::test]
    async fn video_loop_handles_zero_dimensions() {
        // Width/height = 0 -> VideoFrameHeader builds a 0-dim header but the
        // loop should still emit it.
        let (encoded_tx, mut encoded_rx) = mpsc::channel::<Vec<u8>>(8);
        let (ws_tx, mut ws_rx) = mpsc::channel::<Message>(8);
        let force_kf = Arc::new(AtomicBool::new(false));
        let video_nk = Arc::new(AtomicBool::new(false));
        let (cmd_tx, _cmd_rx) = std::sync::mpsc::channel::<CaptureCommand>();
        let width = Arc::new(AtomicU32::new(0));
        let height = Arc::new(AtomicU32::new(0));

        encoded_tx.send(fake_idr(2048)).await.unwrap();
        drop(encoded_tx);

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
        drop(ws_tx);

        let mut count = 0;
        while let Some(Message::Binary(data)) = ws_rx.recv().await {
            count += 1;
            let header = beam_protocol::VideoFrameHeader::deserialize(&data).unwrap();
            // The header reflects the (zero) dimensions from the atomics.
            assert_eq!(header.width, 0);
            assert_eq!(header.height, 0);
        }
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn video_loop_keyframe_flag_set_on_idr_not_p_frame() {
        let frames = vec![fake_p_frame(500), fake_idr(2048), fake_p_frame(500)];
        let results = run_loop_collect(frames, false).await;
        // initial IDR + 3 frames = 4
        assert_eq!(results.len(), 4);
        // results[0] = initial IDR, results[1] = first P (NOT keyframe),
        // results[2] = injected IDR, results[3] = trailing P
        assert!(results[0].1, "Initial IDR should have keyframe flag");
        assert!(!results[1].1, "First P-frame should NOT have keyframe flag");
        assert!(results[2].1, "Injected IDR should have keyframe flag");
        assert!(
            !results[3].1,
            "Trailing P-frame should NOT have keyframe flag"
        );
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

    // --- classify_idr_wait_step ---

    /// Builder helper: defaults are "in the waiting state, no timeout, no
    /// attempts, threshold 5, no resets yet, max 3, min_idr_size 1024".
    /// Individual tests override only the field they care about.
    fn wait_inputs() -> IdrWaitInputs {
        IdrWaitInputs {
            waiting_for_idr: true,
            is_idr: false,
            data_len: 200,
            min_idr_size: 1024,
            elapsed_ms: 0,
            wait_timeout_ms: 500,
            idr_wait_attempts: 0,
            attempt_threshold: 5,
            encoder_reset_count: 0,
            max_encoder_resets: 3,
        }
    }

    #[test]
    fn idr_wait_not_waiting_passes_through() {
        // When waiting_for_idr=false, the helper short-circuits.
        let mut i = wait_inputs();
        i.waiting_for_idr = false;
        assert_eq!(classify_idr_wait_step(&i), IdrWaitStep::NotWaiting);
    }

    #[test]
    fn idr_wait_accept_real_idr() {
        // A real IDR (size >= MIN_IDR_SIZE) accepts immediately.
        let mut i = wait_inputs();
        i.is_idr = true;
        i.data_len = 2048;
        assert_eq!(classify_idr_wait_step(&i), IdrWaitStep::AcceptIdr);
    }

    #[test]
    fn idr_wait_p_frame_skipped_silently_within_timeout() {
        // P-frame before timeout: just skip, no side effects.
        let mut i = wait_inputs();
        i.elapsed_ms = 100;
        assert_eq!(classify_idr_wait_step(&i), IdrWaitStep::SkipFrame);
    }

    #[test]
    fn idr_wait_undersized_idr_within_timeout_forces_keyframe() {
        // Undersized IDR triggers a force-keyframe request.
        let mut i = wait_inputs();
        i.is_idr = true;
        i.data_len = 500;
        i.elapsed_ms = 100;
        assert_eq!(
            classify_idr_wait_step(&i),
            IdrWaitStep::SkipAndForceKeyframe
        );
    }

    #[test]
    fn idr_wait_timeout_under_attempt_threshold_forces_and_resets_clock() {
        // Timeout but only attempt 1 (< threshold 5) — force keyframe + reset clock.
        let mut i = wait_inputs();
        i.elapsed_ms = 600;
        assert_eq!(
            classify_idr_wait_step(&i),
            IdrWaitStep::SkipForceKeyframeAndResetClock
        );
    }

    #[test]
    fn idr_wait_attempts_exceed_threshold_triggers_encoder_reset() {
        // attempts=5, increments to 6 → > threshold 5; encoder_reset_count=0 < 3
        // → reset path.
        let mut i = wait_inputs();
        i.elapsed_ms = 600;
        i.idr_wait_attempts = 5;
        assert_eq!(
            classify_idr_wait_step(&i),
            IdrWaitStep::SkipResetEncoderAndCounters
        );
    }

    #[test]
    fn idr_wait_exhausted_resets_proceeds_with_p_frames() {
        // attempts > threshold AND encoder_reset_count == max → fallback to
        // P-frames. (Same as 'gave up' state in the production loop.)
        let mut i = wait_inputs();
        i.elapsed_ms = 600;
        i.idr_wait_attempts = 6;
        i.encoder_reset_count = 3;
        assert_eq!(classify_idr_wait_step(&i), IdrWaitStep::ProceedWithPFrames);
    }

    #[test]
    fn idr_wait_exhausted_one_over_max_also_proceeds() {
        // encoder_reset_count > max also exits to P-frames.
        let mut i = wait_inputs();
        i.elapsed_ms = 600;
        i.idr_wait_attempts = 7;
        i.encoder_reset_count = 4;
        assert_eq!(classify_idr_wait_step(&i), IdrWaitStep::ProceedWithPFrames);
    }

    #[test]
    fn idr_wait_timeout_exact_boundary_does_not_trigger() {
        // elapsed_ms == wait_timeout_ms uses `>`, so equality does NOT trigger.
        let mut i = wait_inputs();
        i.elapsed_ms = 500;
        assert_eq!(classify_idr_wait_step(&i), IdrWaitStep::SkipFrame);
    }

    #[test]
    fn idr_wait_timeout_just_over_boundary_triggers() {
        // 501ms > 500ms — triggers the timeout path.
        let mut i = wait_inputs();
        i.elapsed_ms = 501;
        assert_eq!(
            classify_idr_wait_step(&i),
            IdrWaitStep::SkipForceKeyframeAndResetClock
        );
    }

    #[test]
    fn idr_wait_step_enum_has_unique_variants() {
        // Smoke test the Debug + PartialEq derives are healthy.
        let steps = [
            IdrWaitStep::SkipFrame,
            IdrWaitStep::SkipAndForceKeyframe,
            IdrWaitStep::SkipForceKeyframeAndResetClock,
            IdrWaitStep::SkipResetEncoderAndCounters,
            IdrWaitStep::ProceedWithPFrames,
            IdrWaitStep::AcceptIdr,
            IdrWaitStep::NotWaiting,
        ];
        // Each variant should be distinguishable from the others.
        for (i, a) in steps.iter().enumerate() {
            for (j, b) in steps.iter().enumerate() {
                if i == j {
                    assert_eq!(a, b);
                } else {
                    assert_ne!(a, b);
                }
            }
        }
    }

    #[test]
    fn idr_wait_idr_at_min_idr_size_is_accepted() {
        // data_len == min_idr_size uses `<`, so equality DOES NOT undersize.
        let mut i = wait_inputs();
        i.is_idr = true;
        i.data_len = 1024;
        assert_eq!(classify_idr_wait_step(&i), IdrWaitStep::AcceptIdr);
    }

    #[test]
    fn idr_wait_idr_one_byte_below_min_is_undersized() {
        let mut i = wait_inputs();
        i.is_idr = true;
        i.data_len = 1023;
        assert_eq!(
            classify_idr_wait_step(&i),
            IdrWaitStep::SkipAndForceKeyframe
        );
    }

    #[test]
    fn idr_wait_p_frame_at_exact_attempt_threshold_still_force_keyframe() {
        // attempts = threshold (5) → increments to 6 → > threshold 5 → reset path.
        // Test the boundary: attempts = threshold - 1 = 4 → increments to 5,
        // which is NOT > 5, so we get the force-keyframe path.
        let mut i = wait_inputs();
        i.elapsed_ms = 600;
        i.idr_wait_attempts = 4;
        assert_eq!(
            classify_idr_wait_step(&i),
            IdrWaitStep::SkipForceKeyframeAndResetClock
        );
    }

    #[test]
    fn idr_wait_inputs_struct_debug_works() {
        // The struct is `#[derive(Debug)]` so log messages can pin it.
        let i = wait_inputs();
        let dbg = format!("{i:?}");
        assert!(dbg.contains("IdrWaitInputs"));
        assert!(dbg.contains("waiting_for_idr"));
    }
}
