use crate::CaptureCommand;
use crate::encoder;
use crate::peer::{self, SharedPeer};

use std::time::Duration;
use tracing::{debug, info};

/// Run the adaptive bitrate control loop.
///
/// NVIDIA: disabled at runtime -- nvh264enc on ARM64 corrupts colors
/// when bitrate is changed dynamically via set_property.
pub(crate) async fn run_abr_loop(
    shared_peer: &SharedPeer,
    encoder_type: encoder::EncoderType,
    initial_bitrate: u32,
    min_bitrate: u32,
    max_bitrate: u32,
    capture_cmd_tx: &std::sync::mpsc::Sender<CaptureCommand>,
) {
    let mut current_bitrate = initial_bitrate;
    let abr_enabled = !matches!(encoder_type, encoder::EncoderType::Nvidia);
    if !abr_enabled {
        info!(
            "Adaptive bitrate disabled for NVIDIA encoder (runtime changes cause color corruption)"
        );
    }
    let mut loss_ema: f64 = 0.0;
    let mut prev_packets_sent: u64 = 0;
    let mut prev_packets_lost: i64 = 0;

    loop {
        tokio::time::sleep(Duration::from_secs(1)).await;

        let current_peer = peer::snapshot(shared_peer).await;
        if !current_peer.is_connected() {
            continue;
        }

        let stats = current_peer.get_stats().await;
        let mut total_packets_sent: u64 = 0;
        let mut total_packets_lost: i64 = 0;
        let mut rtt_sum: f64 = 0.0;
        let mut rtt_count: u32 = 0;

        for (_key, stat) in stats.reports.iter() {
            use webrtc::stats::StatsReportType;
            if let StatsReportType::OutboundRTP(rtp) = stat
                && rtp.kind == "video"
            {
                total_packets_sent = rtp.packets_sent;
            }
            if let StatsReportType::RemoteInboundRTP(remote) = stat
                && remote.kind == "video"
            {
                total_packets_lost = remote.packets_lost;
                if let Some(rtt) = remote.round_trip_time {
                    rtt_sum += rtt;
                    rtt_count += 1;
                }
            }
        }

        let interval_sent = total_packets_sent.saturating_sub(prev_packets_sent);
        let interval_lost = total_packets_lost.saturating_sub(prev_packets_lost);
        prev_packets_sent = total_packets_sent;
        prev_packets_lost = total_packets_lost;

        let loss_rate = if interval_sent > 0 {
            interval_lost.max(0) as f64 / interval_sent as f64
        } else {
            0.0
        };
        loss_ema = loss_ema * 0.7 + loss_rate * 0.3;

        let avg_rtt = if rtt_count > 0 {
            rtt_sum / rtt_count as f64
        } else {
            0.0
        };

        debug!(
            packets_sent = total_packets_sent,
            packets_lost = total_packets_lost,
            rtt_ms = format!("{:.0}", avg_rtt * 1000.0),
            loss_pct = format!("{:.1}", loss_ema * 100.0),
            bitrate = current_bitrate,
            "WebRTC stats"
        );

        if !abr_enabled {
            continue;
        }

        let new_bitrate = if loss_ema > 0.05 {
            ((current_bitrate as f64 * 0.7) as u32).max(min_bitrate)
        } else if loss_ema < 0.01 && avg_rtt < 0.05 {
            ((current_bitrate as f64 * 1.5) as u32).min(max_bitrate)
        } else if loss_ema < 0.01 {
            ((current_bitrate as f64 * 1.2) as u32).min(max_bitrate)
        } else {
            current_bitrate
        };

        if new_bitrate != current_bitrate {
            info!(
                old = current_bitrate,
                new = new_bitrate,
                loss_pct = format!("{:.1}", loss_ema * 100.0),
                rtt_ms = format!("{:.0}", avg_rtt * 1000.0),
                "Adaptive bitrate adjustment"
            );
            let _ = capture_cmd_tx.send(CaptureCommand::SetBitrate(new_bitrate));
            current_bitrate = new_bitrate;
        }
    }
}
