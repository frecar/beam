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

    // --- Header / type lines always emitted ---

    #[test]
    fn render_prometheus_emits_all_help_and_type_lines_when_empty() {
        let store = ClientMetricsStore::default();
        let body = store.render_prometheus(&[]);

        // Every metric must have its HELP + TYPE header regardless of active sessions
        let expected_headers = [
            "beam_client_latency_ms",
            "beam_client_jitter_ms",
            "beam_client_fps",
            "beam_client_decode_ms",
            "beam_client_video_bytes_per_second",
            "beam_client_audio_bytes_per_second",
            "beam_client_video_frames_decoded_total",
            "beam_client_video_frames_dropped_total",
            "beam_client_audio_frames_decoded_total",
            "beam_client_audio_dropouts_total",
            "beam_client_audio_buffer_delay_ms",
            "beam_client_last_report_age_seconds",
        ];
        for name in expected_headers {
            assert!(
                body.contains(&format!("# HELP {name}")),
                "Missing HELP line for {name}"
            );
            assert!(
                body.contains(&format!("# TYPE {name}")),
                "Missing TYPE line for {name}"
            );
        }
    }

    #[test]
    fn render_prometheus_counter_types_distinct_from_gauges() {
        let store = ClientMetricsStore::default();
        let body = store.render_prometheus(&[]);
        // Counters are *_total
        assert!(body.contains("# TYPE beam_client_video_frames_decoded_total counter"));
        assert!(body.contains("# TYPE beam_client_audio_dropouts_total counter"));
        // Gauges
        assert!(body.contains("# TYPE beam_client_latency_ms gauge"));
        assert!(body.contains("# TYPE beam_client_fps gauge"));
    }

    // --- Optional metric branches: Some vs None ---

    #[test]
    fn render_prometheus_omits_none_optional_metrics() {
        let store = ClientMetricsStore::default();
        let session_id = Uuid::nil();
        // All optional fields are None — only counters/gauges should appear, optionals skipped
        store.update(
            session_id,
            ClientMetricsReport {
                latency_ms: None,
                jitter_ms: None,
                fps: None,
                decode_ms: None,
                video_bytes_per_second: 5,
                audio_bytes_per_second: 6,
                video_frames_decoded_total: 7,
                video_frames_dropped_total: 8,
                audio_frames_decoded_total: 9,
                audio_dropouts_total: 10,
                audio_buffer_delay_ms: None,
            },
        );

        let body = store.render_prometheus(&[session(session_id)]);

        // Required (always-present) metrics still rendered
        assert!(body.contains(
            "beam_client_video_bytes_per_second{session=\"00000000-0000-0000-0000-000000000000\"} 5"
        ));
        // Optional metrics with None value should NOT appear with a value
        let session_str = "session=\"00000000-0000-0000-0000-000000000000\"";
        for name in [
            "beam_client_latency_ms",
            "beam_client_jitter_ms",
            "beam_client_fps",
            "beam_client_decode_ms",
            "beam_client_audio_buffer_delay_ms",
        ] {
            let line = format!("{name}{{{session_str}}}");
            assert!(
                !body.contains(&line),
                "Expected no value line for None optional {name}, got body: {body}"
            );
        }
    }

    // --- f64 finiteness guard ---

    #[test]
    fn render_prometheus_skips_non_finite_optionals() {
        let store = ClientMetricsStore::default();
        let session_id = Uuid::nil();
        store.update(
            session_id,
            ClientMetricsReport {
                // NaN, +inf, -inf must all be skipped by the is_finite() guard
                latency_ms: Some(f64::NAN),
                jitter_ms: Some(f64::INFINITY),
                fps: Some(f64::NEG_INFINITY),
                decode_ms: Some(1.5),
                video_bytes_per_second: 0,
                audio_bytes_per_second: 0,
                video_frames_decoded_total: 0,
                video_frames_dropped_total: 0,
                audio_frames_decoded_total: 0,
                audio_dropouts_total: 0,
                audio_buffer_delay_ms: Some(2.25),
            },
        );

        let body = store.render_prometheus(&[session(session_id)]);

        // Finite values are emitted
        assert!(body.contains(
            "beam_client_decode_ms{session=\"00000000-0000-0000-0000-000000000000\"} 1.500"
        ));
        assert!(body.contains("beam_client_audio_buffer_delay_ms{session=\"00000000-0000-0000-0000-000000000000\"} 2.250"));
        // NaN/Inf values are silently dropped
        assert!(!body.contains("NaN"), "Body should never serialize NaN");
        assert!(!body.contains("inf"), "Body should never serialize inf");
    }

    // --- remove() path ---

    #[test]
    fn remove_clears_snapshot_for_session() {
        let store = ClientMetricsStore::default();
        let session_id = Uuid::new_v4();
        store.update(
            session_id,
            ClientMetricsReport {
                latency_ms: Some(42.0),
                ..Default::default()
            },
        );

        // Confirm metric is present before remove
        let body_before = store.render_prometheus(&[session(session_id)]);
        let label = format!("session=\"{session_id}\"");
        assert!(
            body_before.contains(&format!("beam_client_latency_ms{{{label}}}")),
            "Pre-remove body should contain the session's latency line"
        );

        store.remove(session_id);

        let body_after = store.render_prometheus(&[session(session_id)]);
        assert!(
            !body_after.contains(&format!("beam_client_latency_ms{{{label}}}")),
            "Post-remove body should not contain the session's latency line"
        );
    }

    #[test]
    fn remove_nonexistent_session_is_noop() {
        let store = ClientMetricsStore::default();
        // Removing a session never inserted should not panic or alter behavior
        store.remove(Uuid::new_v4());
        let body = store.render_prometheus(&[]);
        // Still emits headers
        assert!(body.contains("# HELP beam_client_latency_ms"));
    }

    // --- update() overwrites previous snapshot ---

    #[test]
    fn update_overwrites_previous_snapshot() {
        let store = ClientMetricsStore::default();
        let session_id = Uuid::nil();
        store.update(
            session_id,
            ClientMetricsReport {
                video_frames_decoded_total: 1,
                ..Default::default()
            },
        );
        store.update(
            session_id,
            ClientMetricsReport {
                video_frames_decoded_total: 999,
                ..Default::default()
            },
        );

        let body = store.render_prometheus(&[session(session_id)]);
        assert!(body.contains("beam_client_video_frames_decoded_total{session=\"00000000-0000-0000-0000-000000000000\"} 999"));
        // Previous value gone
        assert!(!body.contains("beam_client_video_frames_decoded_total{session=\"00000000-0000-0000-0000-000000000000\"} 1\n"));
    }

    // --- Multi-session rendering ---

    #[test]
    fn render_prometheus_handles_multiple_active_sessions() {
        let store = ClientMetricsStore::default();
        let a = Uuid::from_u128(0x11);
        let b = Uuid::from_u128(0x22);
        store.update(
            a,
            ClientMetricsReport {
                latency_ms: Some(10.0),
                video_frames_decoded_total: 100,
                ..Default::default()
            },
        );
        store.update(
            b,
            ClientMetricsReport {
                latency_ms: Some(20.0),
                video_frames_decoded_total: 200,
                ..Default::default()
            },
        );

        let body = store.render_prometheus(&[session(a), session(b)]);

        let label_a = format!("session=\"{a}\"");
        let label_b = format!("session=\"{b}\"");
        assert!(body.contains(&format!("beam_client_latency_ms{{{label_a}}} 10.000")));
        assert!(body.contains(&format!("beam_client_latency_ms{{{label_b}}} 20.000")));
        assert!(body.contains(&format!(
            "beam_client_video_frames_decoded_total{{{label_a}}} 100"
        )));
        assert!(body.contains(&format!(
            "beam_client_video_frames_decoded_total{{{label_b}}} 200"
        )));
    }

    #[test]
    fn render_prometheus_evicts_only_stale_keeps_active() {
        let store = ClientMetricsStore::default();
        let active = Uuid::from_u128(0xA);
        let stale = Uuid::from_u128(0xB);
        store.update(
            active,
            ClientMetricsReport {
                latency_ms: Some(5.0),
                ..Default::default()
            },
        );
        store.update(
            stale,
            ClientMetricsReport {
                latency_ms: Some(99.0),
                ..Default::default()
            },
        );

        // Only `active` is in the active list — stale must be evicted from internal store too
        let body = store.render_prometheus(&[session(active)]);

        let active_label = format!("session=\"{active}\"");
        let stale_label = format!("session=\"{stale}\"");
        assert!(body.contains(&format!("beam_client_latency_ms{{{active_label}}} 5.000")));
        assert!(!body.contains(&format!("beam_client_latency_ms{{{stale_label}}}")));

        // After eviction, re-rendering with no sessions should still not have the stale entry
        let body2 = store.render_prometheus(&[]);
        assert!(!body2.contains(&format!("beam_client_latency_ms{{{stale_label}}}")));
    }

    // --- last_report_age always >=0 finite ---

    #[test]
    fn render_prometheus_includes_last_report_age() {
        let store = ClientMetricsStore::default();
        let session_id = Uuid::nil();
        store.update(session_id, ClientMetricsReport::default());

        let body = store.render_prometheus(&[session(session_id)]);

        // Age line is present (always finite, derived from updated_at.elapsed())
        let prefix =
            "beam_client_last_report_age_seconds{session=\"00000000-0000-0000-0000-000000000000\"}";
        assert!(
            body.contains(prefix),
            "Expected last_report_age line for session, body: {body}"
        );
    }
}
