use crate::CaptureCommand;
use crate::h264;
use crate::peer::{self, SharedPeer};

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Write encoded video frames to WebRTC (only when connected).
/// Handles IDR wait, peer generation tracking, and health checks.
pub(crate) async fn run_video_send_loop(
    encoded_rx: &mut mpsc::Receiver<Vec<u8>>,
    shared_peer: &SharedPeer,
    force_keyframe: &Arc<AtomicBool>,
    capture_cmd_tx: &std::sync::mpsc::Sender<CaptureCommand>,
    config_framerate: u32,
) {
    let frame_duration = Duration::from_nanos(1_000_000_000u64 / config_framerate as u64);
    let mut video_frame_count: u64 = 0;
    let mut dropped_count: u64 = 0;
    let mut was_connected = false;
    let mut waiting_for_idr = false;
    let mut idr_wait_start = Instant::now();
    let mut idr_wait_attempts: u32 = 0;
    let mut error_count: u64 = 0;
    let mut encoder_reset_count: u32 = 0;
    const MAX_ENCODER_RESETS: u32 = 3;
    let mut current_peer_gen: u64 = 0;
    // Health-check: frames written since last packets_sent check
    let mut frames_since_health_check: u64 = 0;
    let mut last_health_check = Instant::now();
    while let Some(data) = encoded_rx.recv().await {
        let (current_peer, peer_gen) = peer::snapshot_with_gen(shared_peer).await;

        // Detect peer swap: unconditionally reset video loop state.
        // This is the critical fix: the old pattern relied on detecting
        // an is_connected() false->true transition, which can be missed
        // if the swap + reconnect happens between loop iterations.
        if peer_gen != current_peer_gen {
            if current_peer_gen != 0 {
                info!(
                    old_peer_gen = current_peer_gen,
                    new_peer_gen = peer_gen,
                    "Peer generation changed, resetting video loop state"
                );
            }
            current_peer_gen = peer_gen;
            was_connected = false;
            waiting_for_idr = false;
            dropped_count = 0;
            video_frame_count = 0;
            error_count = 0;
            encoder_reset_count = 0;
            frames_since_health_check = 0;
            last_health_check = Instant::now();
        }

        // Skip frames when WebRTC isn't connected yet
        if !current_peer.is_connected() {
            dropped_count += 1;
            was_connected = false;
            if dropped_count == 1 || dropped_count.is_multiple_of(300) {
                debug!(
                    dropped_count,
                    peer_gen, "Dropping video frame (not connected)"
                );
            }
            continue;
        }

        // Force IDR keyframe on first connected frame so the browser
        // decoder can initialize.
        if !was_connected {
            info!(
                dropped_before_connect = dropped_count,
                peer_gen, "WebRTC connected, forcing IDR keyframe"
            );
            force_keyframe.store(true, Ordering::Relaxed);
            was_connected = true;
            waiting_for_idr = true;
            idr_wait_start = Instant::now();
            idr_wait_attempts = 0;
            error_count = 0;
            frames_since_health_check = 0;
            last_health_check = Instant::now();
            // Don't `continue` here -- the current frame may already be
            // an IDR from a freshly recreated encoder. Let it fall
            // through to the waiting_for_idr check below.
        }

        let is_idr = h264::h264_contains_idr(&data);

        if waiting_for_idr {
            if !is_idr {
                // Timeout: if IDR hasn't arrived within 500ms, force
                // another keyframe. Bail after 5 attempts to avoid
                // spinning forever if the encoder can't produce IDR.
                if idr_wait_start.elapsed() > Duration::from_millis(500) {
                    idr_wait_attempts += 1;
                    if idr_wait_attempts > 5 {
                        if encoder_reset_count < MAX_ENCODER_RESETS {
                            encoder_reset_count += 1;
                            warn!(
                                attempts = idr_wait_attempts,
                                reset = encoder_reset_count,
                                max_resets = MAX_ENCODER_RESETS,
                                peer_gen,
                                "Failed to get IDR, resetting encoder pipeline"
                            );
                            let _ = capture_cmd_tx.send(CaptureCommand::ResetEncoder);
                            idr_wait_start = Instant::now();
                            idr_wait_attempts = 0;
                        } else {
                            error!(
                                resets = encoder_reset_count,
                                peer_gen, "Exhausted encoder resets, proceeding with P-frames"
                            );
                            waiting_for_idr = false;
                        }
                    } else {
                        info!(
                            attempt = idr_wait_attempts,
                            waited_ms = idr_wait_start.elapsed().as_millis() as u64,
                            peer_gen,
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
                peer_gen,
                "First IDR frame after connect"
            );
            waiting_for_idr = false;
        }

        let data_len = data.len();
        if video_frame_count < 5 {
            let hex_preview: String = data
                .iter()
                .take(32)
                .map(|b| format!("{:02x}", b))
                .collect::<Vec<_>>()
                .join(" ");
            debug!(size = data_len, is_idr, hex = %hex_preview, "Frame NAL bytes (pre-write)");
        }
        match current_peer
            .write_video_sample(data, frame_duration.as_nanos() as u64)
            .await
        {
            Ok(()) => {
                video_frame_count += 1;
                frames_since_health_check += 1;
                if video_frame_count <= 5 {
                    info!(
                        size = data_len,
                        is_idr,
                        frame = video_frame_count,
                        peer_gen,
                        "Video frame written to WebRTC"
                    );
                }
                if video_frame_count.is_multiple_of(300) {
                    info!(
                        video_frame_count,
                        peer_gen, "Video frames written to WebRTC"
                    );
                }

                // Health check: after writing frames for 5+ seconds,
                // verify that RTP packets are actually being sent.
                // webrtc-rs silently returns Ok from write_sample when
                // the track's internal write pipeline is broken (no
                // packetizer, empty bindings, or paused sender).
                if frames_since_health_check >= 150
                    && last_health_check.elapsed() >= Duration::from_secs(5)
                {
                    let packets = current_peer.video_packets_sent().await;
                    if packets == 0 {
                        warn!(
                            frames_written = frames_since_health_check,
                            peer_gen,
                            "STUCK PEER: wrote {} frames but packets_sent=0. \
                             RTP pipeline is broken (silent write_sample drop). \
                             Forcing keyframe and waiting for next peer swap.",
                            frames_since_health_check
                        );
                        // Force a keyframe in case the encoder state is stale
                        force_keyframe.store(true, Ordering::Relaxed);
                        // Reset so we re-check later
                        was_connected = false;
                        waiting_for_idr = false;
                    } else {
                        debug!(
                            packets,
                            frames_since_health_check, "Video health check passed"
                        );
                    }
                    frames_since_health_check = 0;
                    last_health_check = Instant::now();
                }
            }
            Err(e) => {
                error_count += 1;
                if error_count <= 3 || error_count.is_multiple_of(100) {
                    warn!(error_count, peer_gen, "Write video sample: {e:#}");
                }
            }
        }
    }
    info!("Video frame channel closed");
}

/// Write encoded audio frames to WebRTC.
pub(crate) async fn run_audio_send_loop(
    audio_rx: &mut mpsc::Receiver<Vec<u8>>,
    shared_peer: &SharedPeer,
) {
    let audio_duration_ns = Duration::from_millis(20).as_nanos() as u64;
    while let Some(data) = audio_rx.recv().await {
        let current_peer = peer::snapshot(shared_peer).await;
        if let Err(e) = current_peer
            .write_audio_sample(&data, audio_duration_ns)
            .await
        {
            debug!("Write audio sample: {e:#}");
        }
    }
    info!("Audio frame channel closed");
}
