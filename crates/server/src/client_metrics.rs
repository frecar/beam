use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::Mutex;
use std::time::Instant;

use beam_protocol::{ClientMetricsReport, SessionInfo};
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct ClientMetricsSnapshot {
    pub report: ClientMetricsReport,
    pub updated_at: Instant,
}

#[derive(Default)]
pub struct ClientMetricsStore {
    snapshots: Mutex<HashMap<Uuid, ClientMetricsSnapshot>>,
}

impl ClientMetricsStore {
    pub fn update(&self, session_id: Uuid, report: ClientMetricsReport) {
        let mut snapshots = self.snapshots.lock().unwrap_or_else(|e| e.into_inner());
        snapshots.insert(
            session_id,
            ClientMetricsSnapshot {
                report,
                updated_at: Instant::now(),
            },
        );
    }

    pub fn remove(&self, session_id: Uuid) {
        let mut snapshots = self.snapshots.lock().unwrap_or_else(|e| e.into_inner());
        snapshots.remove(&session_id);
    }

    pub fn render_prometheus(&self, sessions: &[SessionInfo]) -> String {
        let active: HashSet<Uuid> = sessions.iter().map(|s| s.id).collect();
        let snapshots = {
            let mut guard = self.snapshots.lock().unwrap_or_else(|e| e.into_inner());
            guard.retain(|session_id, _| active.contains(session_id));
            guard
                .iter()
                .map(|(session_id, snapshot)| (*session_id, snapshot.clone()))
                .collect::<Vec<_>>()
        };

        let mut body = String::new();
        metric_header(
            &mut body,
            "beam_client_latency_ms",
            "gauge",
            "Browser-observed WebSocket round-trip latency in milliseconds",
        );
        metric_header(
            &mut body,
            "beam_client_jitter_ms",
            "gauge",
            "Browser-observed latency jitter in milliseconds",
        );
        metric_header(
            &mut body,
            "beam_client_fps",
            "gauge",
            "Browser-observed decoded video frames per second",
        );
        metric_header(
            &mut body,
            "beam_client_decode_ms",
            "gauge",
            "Browser-observed video decode time in milliseconds",
        );
        metric_header(
            &mut body,
            "beam_client_video_bytes_per_second",
            "gauge",
            "Video bytes received by the browser per second",
        );
        metric_header(
            &mut body,
            "beam_client_audio_bytes_per_second",
            "gauge",
            "Audio bytes received by the browser per second",
        );
        metric_header(
            &mut body,
            "beam_client_video_frames_decoded_total",
            "counter",
            "Total video frames decoded by the browser for this session",
        );
        metric_header(
            &mut body,
            "beam_client_video_frames_dropped_total",
            "counter",
            "Total video frames dropped by the browser before decode for this session",
        );
        metric_header(
            &mut body,
            "beam_client_audio_frames_decoded_total",
            "counter",
            "Total audio frames decoded by the browser for this session",
        );
        metric_header(
            &mut body,
            "beam_client_audio_dropouts_total",
            "counter",
            "Total browser audio scheduling catch-ups for this session",
        );
        metric_header(
            &mut body,
            "beam_client_audio_buffer_delay_ms",
            "gauge",
            "Browser audio playback buffer delay in milliseconds",
        );
        metric_header(
            &mut body,
            "beam_client_last_report_age_seconds",
            "gauge",
            "Age of the latest browser metrics report in seconds",
        );

        for (session_id, snapshot) in snapshots {
            let labels = format!("session=\"{session_id}\"");
            push_optional_metric(
                &mut body,
                "beam_client_latency_ms",
                &labels,
                snapshot.report.latency_ms,
            );
            push_optional_metric(
                &mut body,
                "beam_client_jitter_ms",
                &labels,
                snapshot.report.jitter_ms,
            );
            push_optional_metric(&mut body, "beam_client_fps", &labels, snapshot.report.fps);
            push_optional_metric(
                &mut body,
                "beam_client_decode_ms",
                &labels,
                snapshot.report.decode_ms,
            );
            push_metric(
                &mut body,
                "beam_client_video_bytes_per_second",
                &labels,
                snapshot.report.video_bytes_per_second,
            );
            push_metric(
                &mut body,
                "beam_client_audio_bytes_per_second",
                &labels,
                snapshot.report.audio_bytes_per_second,
            );
            push_metric(
                &mut body,
                "beam_client_video_frames_decoded_total",
                &labels,
                snapshot.report.video_frames_decoded_total,
            );
            push_metric(
                &mut body,
                "beam_client_video_frames_dropped_total",
                &labels,
                snapshot.report.video_frames_dropped_total,
            );
            push_metric(
                &mut body,
                "beam_client_audio_frames_decoded_total",
                &labels,
                snapshot.report.audio_frames_decoded_total,
            );
            push_metric(
                &mut body,
                "beam_client_audio_dropouts_total",
                &labels,
                snapshot.report.audio_dropouts_total,
            );
            push_optional_metric(
                &mut body,
                "beam_client_audio_buffer_delay_ms",
                &labels,
                snapshot.report.audio_buffer_delay_ms,
            );
            push_f64_metric(
                &mut body,
                "beam_client_last_report_age_seconds",
                &labels,
                snapshot.updated_at.elapsed().as_secs_f64(),
            );
        }

        body
    }
}

fn metric_header(body: &mut String, name: &str, metric_type: &str, help: &str) {
    let _ = writeln!(body, "# HELP {name} {help}");
    let _ = writeln!(body, "# TYPE {name} {metric_type}");
}

fn push_metric(body: &mut String, name: &str, labels: &str, value: u64) {
    let _ = writeln!(body, "{name}{{{labels}}} {value}");
}

fn push_optional_metric(body: &mut String, name: &str, labels: &str, value: Option<f64>) {
    if let Some(value) = value {
        push_f64_metric(body, name, labels, value);
    }
}

fn push_f64_metric(body: &mut String, name: &str, labels: &str, value: f64) {
    if value.is_finite() {
        let _ = writeln!(body, "{name}{{{labels}}} {value:.3}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: Uuid) -> SessionInfo {
        SessionInfo {
            id,
            username: "alice".to_string(),
            display: 10,
            width: 1920,
            height: 1080,
            created_at: 0,
        }
    }

    #[test]
    fn render_prometheus_includes_active_session_metrics_without_username() {
        let store = ClientMetricsStore::default();
        let session_id = Uuid::nil();
        store.update(
            session_id,
            ClientMetricsReport {
                latency_ms: Some(23.5),
                jitter_ms: Some(4.0),
                fps: Some(60.0),
                decode_ms: Some(2.5),
                video_bytes_per_second: 1000,
                audio_bytes_per_second: 200,
                video_frames_decoded_total: 120,
                video_frames_dropped_total: 3,
                audio_frames_decoded_total: 80,
                audio_dropouts_total: 1,
                audio_buffer_delay_ms: Some(25.0),
            },
        );

        let body = store.render_prometheus(&[session(session_id)]);

        assert!(body.contains(
            "beam_client_latency_ms{session=\"00000000-0000-0000-0000-000000000000\"} 23.500"
        ));
        assert!(body.contains("beam_client_video_frames_dropped_total{session=\"00000000-0000-0000-0000-000000000000\"} 3"));
        assert!(!body.contains("alice"));
    }

    #[test]
    fn render_prometheus_drops_stale_sessions() {
        let store = ClientMetricsStore::default();
        store.update(Uuid::nil(), ClientMetricsReport::default());

        let body = store.render_prometheus(&[]);

        assert!(!body.contains("beam_client_latency_ms{session="));
    }
}
