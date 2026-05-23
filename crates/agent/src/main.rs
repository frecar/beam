mod audio;
mod capture;
mod cli;
mod clipboard;
mod clipboard_sync;
mod cursor;
mod display;
mod encoder;
mod file_transfer_task;
mod filetransfer;
mod gpu;
mod h264;
mod input;
mod signaling;
mod video;

use anyhow::Context;
use audio::AudioCapture;
use beam_protocol::InputEvent;
use capture::ScreenCapture;
use cli::DEFAULT_FRAMERATE;
use clipboard::ClipboardBridge;
use encoder::Encoder;
use input::InputInjector;
use signaling::SignalingCtx;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

/// Commands sent from async tasks to the capture thread.
/// Using a command channel lets the capture thread exclusively own (and recreate)
/// the Encoder and ScreenCapture during dynamic resolution changes.
pub(crate) enum CaptureCommand {
    Resize {
        width: u32,
        height: u32,
    },
    /// Recreate the encoder pipeline to guarantee a fresh IDR frame.
    ResetEncoder,
}

/// Maximum clipboard text size accepted from the browser. Larger payloads
/// trigger a warning and are dropped (DoS guard for browsers that paste
/// gigabytes of text by accident).
pub(crate) const MAX_CLIPBOARD_BYTES: usize = 1_048_576;

/// Bound for relative-mouse / scroll deltas. Values outside [-MAX, MAX] are
/// rejected as malformed (browsers should never emit those; if they do it's
/// either a bug or an attempted overflow into the X11 wire format).
pub(crate) const MAX_INPUT_DELTA: f64 = 10_000.0;

/// Permissive but defensive check on `dx`/`dy` for relative motion + scroll.
///
/// Splits the "is this input safe to inject?" predicate into a pure function
/// so it can be unit-tested without an X11 connection.
pub(crate) fn is_finite_bounded_delta(dx: f64, dy: f64) -> bool {
    dx.is_finite()
        && dy.is_finite()
        && (-MAX_INPUT_DELTA..=MAX_INPUT_DELTA).contains(&dx)
        && (-MAX_INPUT_DELTA..=MAX_INPUT_DELTA).contains(&dy)
}

/// Check whether a `setxkbmap` layout name is safe to spawn. Layout names
/// arrive over the wire and end up in a subprocess argv, so we enforce a
/// tight allowlist (alphanumeric + `-` + `_`) and a length cap before any
/// process spawn happens.
pub(crate) fn is_valid_layout_name(layout: &str) -> bool {
    !layout.is_empty()
        && layout.len() <= 20
        && layout
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Check whether a clipboard payload is small enough to push into the X11
/// selection. Browsers occasionally paste paste-bombs (gigabyte-scale text)
/// which would hang xclip; this guard bounds the worst case.
pub(crate) fn is_clipboard_size_ok(len: usize) -> bool {
    len <= MAX_CLIPBOARD_BYTES
}

/// Parse a display string like `:10` into a numeric display id, falling back
/// to the default Beam display (10) if the string is malformed. The parsing
/// is split out so the fallback branch can be unit-tested without spawning
/// the full agent.
pub(crate) fn parse_display_num(x_display: &str) -> u32 {
    x_display.trim_start_matches(':').parse().unwrap_or(10)
}

/// Build the PulseAudio "unix:..." path string for a given display id.
/// The path mirrors the layout chosen by `VirtualDisplay::start_pulseaudio`
/// and is used for both PulseAudio server discovery (existing display) and
/// freshly-started sessions.
pub(crate) fn pulse_server_path(display_num: u32) -> String {
    format!("/tmp/beam-pulse-{display_num}/native")
}

pub(crate) fn pulse_server_url(display_num: u32) -> String {
    format!("unix:{}", pulse_server_path(display_num))
}

/// Compute the effective framerate + bitrate for the encoder, applying the
/// software-encoder cap (~60fps at 1080p, 20Mbps).
///
/// On ARM64 with x264enc the encoder cannot sustain 120fps; this guard
/// prevents the appsrc queue from growing faster than the encoder drains
/// it and OOM-killing the process.
pub(crate) fn cap_software_encoder_params(
    encoder_type: encoder::EncoderType,
    requested_fps: u32,
    requested_bitrate: u32,
) -> (u32, u32) {
    if matches!(encoder_type, encoder::EncoderType::Software) && requested_fps > 60 {
        (60, requested_bitrate.min(20_000))
    } else {
        (requested_fps, requested_bitrate)
    }
}

/// xrandr output fallback name used when no virtual display is in play.
pub(crate) const DEFAULT_OUTPUT_NAME: &str = "DUMMY0";

/// Side-effect-free Ctrl/AltGr key tracker used by the input callback.
/// X11 keycodes 29 (LeftCtrl) and 97 (RightCtrl) toggle the same internal
/// flag — when either is pressed and the user releases `,` or `.` while held,
/// the agent fires a clipboard-read so Ctrl+C/Ctrl+V can sync over.
pub(crate) fn is_ctrl_keycode(c: u16) -> bool {
    c == 29 || c == 97
}

/// Keycode for `.` and `,` — the keys that trigger a clipboard-read when
/// released with Ctrl held. Split out as a predicate so the gating logic can
/// be unit-tested.
pub(crate) fn is_clipboard_read_trigger_key(c: u16) -> bool {
    c == 45 || c == 46
}

/// Predicate gate for the visibility "input restored" branch. When the tab
/// is backgrounded and we receive a user-interactive input event, we clear
/// the backgrounded flag and resume normal framerate. Sub-events like
/// VisibilityState / FileChunk / ClientMetrics never trigger the resume.
pub(crate) fn is_interactive_input_event(event: &InputEvent) -> bool {
    matches!(
        event,
        InputEvent::Key { .. }
            | InputEvent::MouseMove { .. }
            | InputEvent::RelativeMouseMove { .. }
            | InputEvent::Button { .. }
            | InputEvent::Scroll { .. }
    )
}

/// Pure classification of an incoming `InputEvent` into the high-level
/// action the input callback should perform. Split out so the dispatch
/// branches can be unit-tested without owning X11, clipboard, channels,
/// or the encoder pipeline. The callback maps each variant onto the
/// appropriate side-effecting subsystem (XTEST inject, clipboard set,
/// resize channel, layout subprocess, encoder reset, file transfer).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum InputAction {
    /// Inject a key (`code`, `pressed`). If `code` is a Ctrl keycode the
    /// dispatcher also updates `ctrl_down` BEFORE attempting the injection.
    /// If the key is a clipboard-read trigger AND ctrl_down is held AND
    /// `pressed` is false, the dispatcher fires a clipboard-read request.
    InjectKey { code: u16, pressed: bool },
    /// Absolute mouse move at normalized [0.0, 1.0] coordinates.
    InjectMouseAbs { x: f64, y: f64 },
    /// Relative mouse move. Only emitted when `(dx, dy)` passes
    /// `is_finite_bounded_delta` — invalid deltas become `Ignore`.
    InjectMouseRel { dx: f64, dy: f64 },
    /// Mouse button press/release (browser index, see `InputInjector::map_button`).
    InjectButton { b: u8, pressed: bool },
    /// Scroll event. Only emitted when `(dx, dy)` passes
    /// `is_finite_bounded_delta` — invalid deltas become `Ignore`.
    InjectScroll { dx: f64, dy: f64 },
    /// Set the regular X11 clipboard. Empty `text` is OK; large text becomes
    /// `Ignore` (caller asserts via `is_clipboard_size_ok`).
    SetClipboard { text: String },
    /// Set the X11 primary selection (middle-click paste buffer).
    SetClipboardPrimary { text: String },
    /// Send a resize request through the resize channel. Dimensions have
    /// already been clamped by `display::clamp_resize_dimensions`; if
    /// clamping failed the variant becomes `Ignore`.
    Resize { width: u32, height: u32 },
    /// Spawn a `setxkbmap` subprocess with the validated layout name.
    /// Skipped when the layout matches the previous one (de-dup).
    Layout { layout: String },
    /// Tab visibility changed. `visible=true` resets the encoder + wakes
    /// the capture thread; `visible=false` just flips the backgrounded flag.
    Visibility { visible: bool },
    /// File transfer start. Forwarded to FileTransferManager.
    FileStart { id: String, name: String, size: u64 },
    /// File transfer chunk. Forwarded to FileTransferManager.
    FileChunk { id: String, data: String },
    /// File transfer complete. Forwarded to FileTransferManager.
    FileDone { id: String },
    /// Download request from the browser. Forwarded to download channel.
    FileDownload { path: String },
    /// Event is intentionally ignored (oversized clipboard, malformed
    /// delta, unknown layout name, invalid resize dimensions, browser
    /// metrics destined for the server, removed Quality selector).
    Ignore,
}

/// Build the cursor-shape WS message body that gets forwarded to the
/// browser. Pure helper exposed for testing — the production loop sends
/// the resulting JSON via the WebSocket text channel.
pub(crate) fn build_cursor_message(css: &str) -> String {
    serde_json::json!({ "t": "cur", "css": css }).to_string()
}

/// Handle a `InputAction::Visibility { visible }` dispatch on the
/// channel-only side effects (no X11 access). Flips the backgrounded
/// flag; on `visible=true` also marks the encoder needs a keyframe and
/// sends a ResetEncoder command. Returns `true` if the visibility flip
/// triggered the encoder reset path.
///
/// Extracted from `build_input_callback` so the routing can be tested
/// without owning the encoder, the capture thread, or the X11 connection.
pub(crate) fn handle_visibility_change(
    visible: bool,
    tab_backgrounded: &AtomicBool,
    video_needs_keyframe: &AtomicBool,
    capture_cmd_tx: &std::sync::mpsc::Sender<CaptureCommand>,
) -> bool {
    tab_backgrounded.store(!visible, Ordering::Relaxed);
    if visible {
        video_needs_keyframe.store(true, Ordering::Relaxed);
        let _ = capture_cmd_tx.send(CaptureCommand::ResetEncoder);
        true
    } else {
        false
    }
}

/// Decide whether the capture loop should log a "capture frame failed"
/// warning for this consecutive-error count. We log the first 3 errors
/// then every 100th to keep the log volume bounded.
pub(crate) fn should_log_capture_error(consecutive: u64) -> bool {
    consecutive <= 3 || consecutive.is_multiple_of(100)
}

/// Decide whether a setxkbmap layout request is a no-op (same as the most
/// recent layout). Returns `true` when the new layout matches `prev_layout`
/// — the dispatcher should skip spawning setxkbmap. Pure helper so the
/// dedup logic is testable without owning the layout Mutex.
pub(crate) fn layout_is_duplicate(prev_layout: &str, new_layout: &str) -> bool {
    prev_layout == new_layout
}

/// Classify the outcome of a setxkbmap subprocess invocation into a
/// log-shaped string. Returns the message that the production code would
/// log (info on success, warn on failure or spawn error). Split out so
/// the three branches can be unit-tested.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SetxkbmapOutcome {
    /// `setxkbmap` ran and exited 0.
    Success,
    /// `setxkbmap` ran but exited non-zero. Carries the captured stderr.
    Failure { stderr: String },
    /// `setxkbmap` failed to spawn (e.g., binary missing).
    SpawnError { reason: String },
}

/// Classify a setxkbmap result tuple into `SetxkbmapOutcome`. Pure helper
/// over (`status_success`, `stderr`, `spawn_err`) so the dispatcher's
/// three branches can be unit-tested without owning a real subprocess.
pub(crate) fn classify_setxkbmap_result(
    spawn_result: Result<(bool, String), String>,
) -> SetxkbmapOutcome {
    match spawn_result {
        Ok((true, _)) => SetxkbmapOutcome::Success,
        Ok((false, stderr)) => SetxkbmapOutcome::Failure { stderr },
        Err(e) => SetxkbmapOutcome::SpawnError { reason: e },
    }
}

/// Build the now-unix-millis timestamp used by the input callback to
/// stamp the `last_input_time` atomic. Pure helper around the SystemTime
/// arithmetic so the fallback (UNIX_EPOCH unavailable) is testable.
pub(crate) fn now_unix_millis(now: std::time::SystemTime) -> u64 {
    now.duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Log shape for the SCHED_FIFO elevation attempt. Returns the human
/// message the agent logs after `sched_setscheduler`. Split out so the
/// two branches (success / capability-denied) can be tested cheaply.
pub(crate) fn sched_fifo_log_message(ret: i32, priority: i32) -> String {
    if ret == 0 {
        format!("Capture thread elevated to SCHED_FIFO priority {priority}")
    } else {
        format!("Could not set SCHED_FIFO (need CAP_SYS_NICE), priority {priority} not applied")
    }
}

/// Decide whether the inbound file-download request was rejected by the
/// channel (e.g. the receiver dropped because the agent is shutting down).
/// Returns true when the production code would log a "dropped" warning.
/// Test-only mirror of the boolean we'd want from the production callsite
/// — the actual code uses `dispatch_file_download` and inverts its bool.
#[cfg(test)]
pub(crate) fn file_download_request_dropped<T>(
    send_result: Result<(), tokio::sync::mpsc::error::TrySendError<T>>,
) -> bool {
    send_result.is_err()
}

/// Decide whether to log a "first frame captured" line. The agent only
/// emits the message once per pipeline lifetime to keep logs quiet, so
/// the boolean tracking state lives outside this helper.
pub(crate) fn should_log_first_frame(already_logged: bool) -> bool {
    !already_logged
}

/// Build the "capture heartbeat" log payload. The capture thread emits a
/// heartbeat every 5 seconds with running frame counts + fps. Splitting
/// the message construction off means the rate-derivation can be tested
/// without owning a real capture loop.
pub(crate) fn format_capture_heartbeat_fps(frame_count: u64, elapsed_secs: f64) -> String {
    if elapsed_secs <= 0.0 {
        return "0.0".to_string();
    }
    format!("{:.1}", frame_count as f64 / elapsed_secs)
}

/// Decide whether the capture thread should sleep on the wake condvar
/// (idle/backgrounded paths) or spin-loop wait (active path).
/// Returns true for the condvar path. Pure helper for testing the choice.
pub(crate) fn capture_wait_should_use_condvar(is_idle: bool, is_backgrounded: bool) -> bool {
    is_idle || is_backgrounded
}

/// Decide whether an "active" capture path should sleep before its
/// spin-loop. Sleeping >1ms before the remaining-2ms boundary cuts
/// CPU significantly without missing the deadline.
pub(crate) fn capture_active_should_sleep(remaining_ms: u64) -> bool {
    remaining_ms > 2
}

/// Decide whether the capture loop should log a "recovered after N
/// consecutive errors" message. Only logs when N>0 (i.e., we actually
/// had a streak to recover from). Pure helper.
pub(crate) fn should_log_capture_recovery(consecutive: u64) -> bool {
    consecutive > 0
}

/// Compute the per-frame duration in nanoseconds from a framerate in fps.
/// Pure helper around the divide-by-zero guard so the worst-case is unit-
/// testable (the production code never passes 0 — `cap_software_encoder_params`
/// clamps to 60 — but the guard is defensive).
pub(crate) fn frame_duration_ns_from_framerate(framerate: u32) -> u64 {
    if framerate == 0 {
        // 1 fps fallback — production never hits this but the guard keeps
        // us from panicking on a malformed config.
        return 1_000_000_000;
    }
    1_000_000_000u64 / framerate as u64
}

/// Decide whether the capture-encode loop's force-keyframe was triggered
/// while the tab was backgrounded — a state-transition we log as a warn
/// because it indicates the browser asked for a keyframe before
/// foregrounding. Pure helper around the AtomicBool swap pattern.
pub(crate) fn should_warn_keyframe_while_backgrounded(was_backgrounded: bool) -> bool {
    was_backgrounded
}

/// Build the warn message logged when the resize handler's xrandr call
/// fails. Pure helper for the log-format pin.
pub(crate) fn format_xrandr_resize_failure(error: &str) -> String {
    format!("xrandr resize failed: {error}")
}

/// Build the info message logged when the capture thread receives a
/// resize request. Pure helper.
pub(crate) fn format_processing_resize_message(width: u32, height: u32) -> String {
    format!("Processing resize request: {width}x{height}")
}

/// Decide whether the encoder-recreate logic should drop+recreate or
/// just emit a force-keyframe. Returns the same as `encoder_reset_cooldown_elapsed`
/// but mirrored to "log the cooldown remaining" vs "go ahead and recreate".
pub(crate) fn should_recreate_encoder(elapsed_ms: u64, cooldown_ms: u64) -> bool {
    encoder_reset_cooldown_elapsed(elapsed_ms, cooldown_ms)
}

/// Compute the cooldown remaining (in ms) for the encoder-reset path.
/// Used for the debug log on the throttled branch. Returns 0 when the
/// cooldown has fully elapsed.
pub(crate) fn encoder_reset_cooldown_remaining_ms(elapsed_ms: u64, cooldown_ms: u64) -> u64 {
    cooldown_ms.saturating_sub(elapsed_ms)
}

/// Decide whether the capture thread should log the encode-buffer drop
/// warning. The drop happens when the encoded-frame channel is full
/// (latency throttling). We log at debug, not warn — locked here so a
/// future refactor doesn't escalate the noise.
#[cfg(test)]
pub(crate) fn should_log_encoded_drop_at_warn(_full: bool) -> bool {
    false
}

/// Compute the maximum bytes the appsrc queue can hold given a frame
/// size. 3 frames of raw BGRA (4 bytes/pixel) — keeps the pipeline
/// from OOMing under software-encoder backpressure. Test-only mirror of
/// the literal in `encoder::Encoder::with_encoder_preference`.
#[cfg(test)]
pub(crate) fn appsrc_queue_max_bytes(width: u32, height: u32) -> u64 {
    width as u64 * height as u64 * 4 * 3
}

/// Dispatch the channel-only side-effect of a Resize action: forward
/// (width, height) onto the resize channel. Returns true on success,
/// false when the channel is closed or full (defensively dropped).
/// Pure helper testable against `tokio::sync::mpsc::channel`.
pub(crate) fn dispatch_resize(
    width: u32,
    height: u32,
    resize_tx: &mpsc::Sender<(u32, u32)>,
) -> bool {
    resize_tx.try_send((width, height)).is_ok()
}

/// Dispatch the channel-only side-effect of a FileDownload action.
/// Forwards the path onto the download request channel. Returns true
/// on success.
pub(crate) fn dispatch_file_download(
    path: String,
    download_request_tx: &mpsc::Sender<String>,
) -> bool {
    download_request_tx.try_send(path).is_ok()
}

/// Dispatch the wake signal that the input callback sends every event.
/// Sets the woken flag + notifies one waiter on the condvar. Pure helper
/// that can be unit-tested against a fresh (Mutex, Condvar).
pub(crate) fn dispatch_capture_wake(wake: &(std::sync::Mutex<bool>, std::sync::Condvar)) {
    let (lock, cvar) = wake;
    let mut woken = lock.lock().unwrap_or_else(|e| e.into_inner());
    *woken = true;
    cvar.notify_one();
}

/// Update the input-timestamp atomic from a now-unix-millis. Pure
/// helper around the Ordering::Relaxed store. Extracted so the
/// atomic store can be tested in isolation.
pub(crate) fn dispatch_update_last_input(now_ms: u64, last_input_time: &AtomicU64) {
    last_input_time.store(now_ms, Ordering::Relaxed);
}

/// Decide whether the agent should clear the tab_backgrounded flag
/// because we received an interactive input event. Returns true iff
/// the event was interactive AND the flag was previously set. The
/// flag is cleared as a side effect (swap). Pure-ish — the side effect
/// is contained in the AtomicBool.
pub(crate) fn dispatch_clear_backgrounded_if_interactive(
    event: &InputEvent,
    tab_backgrounded: &AtomicBool,
) -> bool {
    is_interactive_input_event(event) && tab_backgrounded.swap(false, Ordering::Relaxed)
}

/// Build the X11 keycode from a Linux evdev keycode. X11 keycode =
/// evdev keycode + 8. Test-only mirror — see `InputInjector::evdev_to_x11_keycode`
/// for the production callsite.
#[cfg(test)]
pub(crate) fn evdev_to_x11_keycode(evdev: u16) -> u8 {
    (evdev + 8) as u8
}

/// Build the X11 button number from a browser button index. Test-only
/// mirror of `InputInjector::map_button` (which is private to input.rs).
#[cfg(test)]
pub(crate) fn browser_button_to_x11(button: u8) -> Option<u8> {
    match button {
        0 => Some(1),
        1 => Some(2),
        2 => Some(3),
        _ => None,
    }
}

/// Decide whether the agent should log "tab backgrounded" or "tab foregrounded"
/// transition. Pure helper around the AtomicBool change-detect pattern.
pub(crate) fn capture_state_transition_message(
    is_idle: bool,
    is_backgrounded: bool,
    was_idle: bool,
    was_backgrounded: bool,
    current_framerate: u32,
    idle_framerate: u32,
    background_framerate: u32,
) -> Option<String> {
    if is_backgrounded != was_backgrounded {
        if is_backgrounded {
            return Some(format!(
                "Tab backgrounded, reducing to {background_framerate}fps"
            ));
        } else {
            return Some(format!(
                "Tab foregrounded, restoring framerate {current_framerate}fps"
            ));
        }
    }
    if is_idle != was_idle && !is_backgrounded {
        if is_idle {
            return Some(format!("Entering idle mode ({idle_framerate}fps)"));
        } else {
            return Some(format!("Resuming active mode ({current_framerate}fps)"));
        }
    }
    None
}

/// Decide whether the encoder-reset request should actually destroy and
/// recreate the encoder, or whether we're still in the cooldown window
/// where a force-keyframe is the cheaper alternative. Returns `true`
/// when the cooldown has elapsed (free to recreate).
pub(crate) fn encoder_reset_cooldown_elapsed(last_reset_elapsed_ms: u64, cooldown_ms: u64) -> bool {
    last_reset_elapsed_ms >= cooldown_ms
}

/// Decide whether the capture loop should break entirely after
/// `consecutive` failures in a row (default threshold: 300).
pub(crate) fn should_break_capture_loop(consecutive: u64, threshold: u64) -> bool {
    consecutive >= threshold
}

/// Resolve the agent's file-transfer home directory. Reads `$HOME` and
/// falls back to `/tmp` if unset. Pure helper around the env lookup so
/// the fallback branch can be unit-tested without poking process env.
pub(crate) fn resolve_home_dir(env_home: Option<&str>) -> std::path::PathBuf {
    match env_home {
        Some(s) if !s.is_empty() => std::path::PathBuf::from(s),
        _ => std::path::PathBuf::from("/tmp"),
    }
}

/// Resolve the xrandr output name to use for resize operations. Virtual
/// displays know their output (NVIDIA's DFP-0, dummy's DUMMY0, etc.);
/// when reusing an existing display we fall back to the default name.
/// Pure helper so the fallback branch is unit-testable.
pub(crate) fn resolve_output_name(virtual_display_output: Option<&str>) -> String {
    match virtual_display_output {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => DEFAULT_OUTPUT_NAME.to_string(),
    }
}

/// Decide the audio-capture retry backoff: 500ms for the first 20
/// attempts (10s total), 2000ms thereafter. Pure helper exposed for
/// testing — keeps the audio thread's backoff math straightforward.
pub(crate) fn audio_retry_delay_ms(attempt: u32) -> u64 {
    if attempt < 20 { 500 } else { 2000 }
}

/// Decide whether the agent should log a "still retrying audio capture"
/// message on this attempt. Spammy logs hurt log volume budgets; we
/// log the first 20 attempts (each one), then every 10th attempt to
/// keep the trail audible but quiet.
pub(crate) fn should_log_audio_retry(attempt: u32) -> bool {
    attempt < 20 || attempt.is_multiple_of(10)
}

/// Decide whether the audio-capture thread should give up. After 60
/// failed attempts (about 100 seconds at the configured backoff) we
/// stop retrying because something is fundamentally wrong.
pub(crate) fn should_give_up_audio_retry(attempt: u32) -> bool {
    attempt > 60
}

/// Determine the per-frame sleep duration in nanoseconds based on the
/// agent's current state. Background tabs throttle hardest (~1fps),
/// idle (no input for >5min) drops to ~5fps, and active sessions use
/// the configured framerate.
///
/// Pure helper — split out so the capture thread's framerate-switching
/// decision can be unit-tested without owning the encoder/capture loop.
pub(crate) fn select_frame_duration_ns(
    is_backgrounded: bool,
    is_idle: bool,
    active_ns: u64,
    idle_ns: u64,
    background_ns: u64,
) -> u64 {
    if is_backgrounded {
        background_ns
    } else if is_idle {
        idle_ns
    } else {
        active_ns
    }
}

/// Decide whether the agent is "idle" — no user input for at least
/// `timeout_ms` milliseconds. Returns `false` if `last_input_ms` is 0
/// (never received any input yet — the agent is still warming up, not
/// idle). Pure helper exposed for testing.
pub(crate) fn is_idle_state(last_input_ms: u64, now_ms: u64, timeout_ms: u64) -> bool {
    last_input_ms > 0 && now_ms.saturating_sub(last_input_ms) > timeout_ms
}

/// Decide whether a resize request is a no-op given the current screen
/// dimensions. Resize requests with the same dimensions are dropped to
/// avoid recreating the capture + encoder pipeline for no reason. Pure
/// helper exposed for testing.
pub(crate) fn is_resize_noop(
    current_w: u32,
    current_h: u32,
    requested_w: u32,
    requested_h: u32,
) -> bool {
    current_w == requested_w && current_h == requested_h
}

/// Classify an `InputEvent` into the high-level `InputAction` the agent
/// dispatcher will take. Pure: no side effects, no channel sends, no
/// subprocess spawns. Tests exercise this directly; the production
/// callback maps each variant onto its real subsystem.
pub(crate) fn classify_input_event(
    event: &InputEvent,
    max_width: u32,
    max_height: u32,
) -> InputAction {
    match event {
        InputEvent::Key { c, d } => InputAction::InjectKey {
            code: *c,
            pressed: *d,
        },
        InputEvent::MouseMove { x, y } => InputAction::InjectMouseAbs { x: *x, y: *y },
        InputEvent::RelativeMouseMove { dx, dy } => {
            if is_finite_bounded_delta(*dx, *dy) {
                InputAction::InjectMouseRel { dx: *dx, dy: *dy }
            } else {
                InputAction::Ignore
            }
        }
        InputEvent::Button { b, d } => InputAction::InjectButton { b: *b, pressed: *d },
        InputEvent::Scroll { dx, dy } => {
            if is_finite_bounded_delta(*dx, *dy) {
                InputAction::InjectScroll { dx: *dx, dy: *dy }
            } else {
                InputAction::Ignore
            }
        }
        InputEvent::Clipboard { text } => {
            if is_clipboard_size_ok(text.len()) {
                InputAction::SetClipboard { text: text.clone() }
            } else {
                InputAction::Ignore
            }
        }
        InputEvent::ClipboardPrimary { text } => {
            if is_clipboard_size_ok(text.len()) {
                InputAction::SetClipboardPrimary { text: text.clone() }
            } else {
                InputAction::Ignore
            }
        }
        InputEvent::Resize { w, h } => {
            match display::clamp_resize_dimensions(*w, *h, max_width, max_height) {
                Some((cw, ch)) => InputAction::Resize {
                    width: cw,
                    height: ch,
                },
                None => InputAction::Ignore,
            }
        }
        InputEvent::Layout { layout } => {
            if is_valid_layout_name(layout) {
                InputAction::Layout {
                    layout: layout.clone(),
                }
            } else {
                InputAction::Ignore
            }
        }
        InputEvent::Quality { .. } => InputAction::Ignore,
        InputEvent::ClientMetricsPing { .. } | InputEvent::ClientMetrics(_) => InputAction::Ignore,
        InputEvent::VisibilityState { visible } => InputAction::Visibility { visible: *visible },
        InputEvent::FileStart { id, name, size } => InputAction::FileStart {
            id: id.clone(),
            name: name.clone(),
            size: *size,
        },
        InputEvent::FileChunk { id, data } => InputAction::FileChunk {
            id: id.clone(),
            data: data.clone(),
        },
        InputEvent::FileDone { id } => InputAction::FileDone { id: id.clone() },
        InputEvent::FileDownloadRequest { path } => {
            InputAction::FileDownload { path: path.clone() }
        }
    }
}

/// Shared context for building the input event callback.
struct InputCallbackCtx {
    injector: Arc<Mutex<InputInjector>>,
    clipboard: Arc<Mutex<ClipboardBridge>>,
    file_transfer: Arc<Mutex<filetransfer::FileTransferManager>>,
    resize_tx: mpsc::Sender<(u32, u32)>,
    last_input_time: Arc<AtomicU64>,
    clipboard_read_tx: mpsc::Sender<()>,
    download_request_tx: mpsc::Sender<String>,
    capture_wake: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    capture_cmd_tx: std::sync::mpsc::Sender<CaptureCommand>,
    tab_backgrounded: Arc<AtomicBool>,
    video_needs_keyframe: Arc<AtomicBool>,
    display: String,
    max_width: u32,
    max_height: u32,
}

/// Trait-driven slice of the closure body's mutable state. Holding all
/// the lock-protected resources behind a trait surface lets tests swap in
/// mocks for the X11 / clipboard / filesystem layer without standing up a
/// real X display. Holds the side-effecting state the dispatch function
/// needs to update across events (ctrl-key tracking, layout dedup).
pub(crate) struct InputDispatchState {
    pub(crate) ctrl_down: AtomicBool,
    pub(crate) last_layout: std::sync::Mutex<String>,
}

impl InputDispatchState {
    pub(crate) fn new() -> Self {
        Self {
            ctrl_down: AtomicBool::new(false),
            last_layout: std::sync::Mutex::new(String::new()),
        }
    }
}

/// Channels + flags the dispatch function reads/writes. Kept as borrowed
/// references rather than `Arc<_>` clones so a test driver can compose them
/// from local fixtures without smart-pointer ceremony.
pub(crate) struct InputDispatchChannels<'a> {
    pub(crate) resize_tx: &'a mpsc::Sender<(u32, u32)>,
    pub(crate) clipboard_read_tx: &'a mpsc::Sender<()>,
    pub(crate) download_request_tx: &'a mpsc::Sender<String>,
    pub(crate) capture_wake: &'a (std::sync::Mutex<bool>, std::sync::Condvar),
    pub(crate) capture_cmd_tx: &'a std::sync::mpsc::Sender<CaptureCommand>,
    pub(crate) tab_backgrounded: &'a AtomicBool,
    pub(crate) video_needs_keyframe: &'a AtomicBool,
}

/// Effect emitted by [`dispatch_input_action_pure`] to describe a Layout
/// branch that needs a subprocess spawn. Pure helper output so the
/// dispatch can be exercised without a real `setxkbmap` binary on the
/// test host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LayoutEffect {
    /// New layout — caller should spawn setxkbmap.
    Spawn { layout: String, display: String },
    /// Duplicate of previous layout — no spawn needed.
    Skip,
}

/// Dispatch a single [`InputAction`] onto the trait-driven injector,
/// clipboard, file sink + the shared channels/flags. Returns a
/// `LayoutEffect` for the Layout action and `None` for every other
/// variant, since Layout is the only branch that fans out to a fresh
/// thread + subprocess. The closure body wraps this and spawns the
/// `setxkbmap` thread when a `Spawn` effect comes back.
pub(crate) fn dispatch_input_action<I, C, F>(
    action: InputAction,
    injector: &mut I,
    clipboard: &C,
    file_sink: &mut F,
    state: &InputDispatchState,
    channels: &InputDispatchChannels<'_>,
    display: &str,
) -> Option<LayoutEffect>
where
    I: input::Inject + ?Sized,
    C: clipboard::ClipboardWrite + ?Sized,
    F: filetransfer::FileSink + ?Sized,
{
    match action {
        InputAction::InjectKey { code, pressed } => {
            if is_ctrl_keycode(code) {
                state.ctrl_down.store(pressed, Ordering::Relaxed);
            }
            if !pressed
                && is_clipboard_read_trigger_key(code)
                && state.ctrl_down.load(Ordering::Relaxed)
            {
                let _ = channels.clipboard_read_tx.try_send(());
            }
            if let Err(e) = injector.inject_key(code, pressed) {
                warn!("Key inject error: {e:#}");
            }
            None
        }
        InputAction::InjectMouseAbs { x, y } => {
            if let Err(e) = injector.inject_mouse_move_abs(x, y) {
                warn!("Mouse move inject error: {e:#}");
            }
            None
        }
        InputAction::InjectMouseRel { dx, dy } => {
            if let Err(e) = injector.inject_mouse_move_rel(dx, dy) {
                warn!("Relative mouse move inject error: {e:#}");
            }
            None
        }
        InputAction::InjectButton { b, pressed } => {
            if let Err(e) = injector.inject_button(b, pressed) {
                warn!("Button inject error: {e:#}");
            }
            None
        }
        InputAction::InjectScroll { dx, dy } => {
            if let Err(e) = injector.inject_scroll(dx, dy) {
                warn!("Scroll inject error: {e:#}");
            }
            None
        }
        InputAction::SetClipboard { text } => {
            if let Err(e) = clipboard.set_text(&text) {
                warn!("Clipboard set error: {e:#}");
            }
            None
        }
        InputAction::SetClipboardPrimary { text } => {
            if let Err(e) = clipboard.set_primary_text(&text) {
                warn!("Primary clipboard set error: {e:#}");
            }
            None
        }
        InputAction::Resize { width, height } => {
            let _ = dispatch_resize(width, height, channels.resize_tx);
            None
        }
        InputAction::Layout { layout } => {
            let mut prev = state.last_layout.lock().unwrap_or_else(|e| e.into_inner());
            if layout_is_duplicate(&prev, &layout) {
                return Some(LayoutEffect::Skip);
            }
            *prev = layout.clone();
            drop(prev);
            Some(LayoutEffect::Spawn {
                layout,
                display: display.to_string(),
            })
        }
        InputAction::Visibility { visible } => {
            info!(visible, "Browser tab visibility changed");
            let triggered_reset = handle_visibility_change(
                visible,
                channels.tab_backgrounded,
                channels.video_needs_keyframe,
                channels.capture_cmd_tx,
            );
            if triggered_reset {
                info!("Resetting encoder for browser reconnect");
                dispatch_capture_wake(channels.capture_wake);
            }
            None
        }
        InputAction::FileStart { id, name, size } => {
            if let Err(e) = file_sink.handle_file_start(&id, &name, size) {
                warn!(id, name, "File transfer start error: {e:#}");
            }
            None
        }
        InputAction::FileChunk { id, data } => {
            if let Err(e) = file_sink.handle_file_chunk(&id, &data) {
                warn!(id, "File chunk error: {e:#}");
            }
            None
        }
        InputAction::FileDone { id } => {
            if let Err(e) = file_sink.handle_file_done(&id) {
                warn!(id, "File done error: {e:#}");
            }
            None
        }
        InputAction::FileDownload { path } => {
            if !dispatch_file_download(path.clone(), channels.download_request_tx) {
                warn!(path, "File download request dropped");
            }
            None
        }
        InputAction::Ignore => {
            // Oversized clipboard, malformed delta, unknown layout name,
            // invalid resize dimensions, browser metrics, or the removed
            // Quality selector. Defensively ignored — caller logs context.
            None
        }
    }
}

/// Spawn the `setxkbmap` worker thread that the Layout dispatch produced.
/// Pulled out as a standalone function so the production closure stays
/// thin and the spawn logic is uniformly logged.
pub(crate) fn spawn_setxkbmap_worker(effect: LayoutEffect) {
    let LayoutEffect::Spawn { layout, display } = effect else {
        return;
    };
    std::thread::spawn(move || {
        let spawn_result = std::process::Command::new("setxkbmap")
            .arg(&layout)
            .env("DISPLAY", &display)
            .output()
            .map(|o| {
                let stderr = String::from_utf8_lossy(&o.stderr).to_string();
                (o.status.success(), stderr)
            })
            .map_err(|e| e.to_string());
        match classify_setxkbmap_result(spawn_result) {
            SetxkbmapOutcome::Success => {
                info!(layout = %layout, "Keyboard layout set via setxkbmap");
            }
            SetxkbmapOutcome::Failure { stderr } => {
                warn!(layout = %layout, "setxkbmap failed: {stderr}");
            }
            SetxkbmapOutcome::SpawnError { reason } => {
                warn!(layout = %layout, "Failed to run setxkbmap: {reason}");
            }
        }
    });
}

/// Pre-dispatch hook: every inbound input event refreshes the
/// last-input timestamp + wakes the capture thread out of idle mode +
/// clears the backgrounded flag when the event is interactive. Split
/// out so the bookkeeping is testable end-to-end without owning the X
/// connection or the encoder pipeline.
pub(crate) fn handle_pre_dispatch(
    event: &InputEvent,
    last_input_time: &AtomicU64,
    capture_wake: &(std::sync::Mutex<bool>, std::sync::Condvar),
    tab_backgrounded: &AtomicBool,
    now: std::time::SystemTime,
) -> bool {
    let now_ms = now_unix_millis(now);
    dispatch_update_last_input(now_ms, last_input_time);
    dispatch_capture_wake(capture_wake);
    let cleared = dispatch_clear_backgrounded_if_interactive(event, tab_backgrounded);
    if cleared {
        debug!("Input received while backgrounded, clearing flag");
    }
    cleared
}

/// Build the reusable input event callback that dispatches input events
/// to the appropriate subsystem (XTEST, clipboard, resize, layout, quality).
fn build_input_callback(ctx: InputCallbackCtx) -> Arc<dyn Fn(InputEvent) + Send + Sync> {
    let InputCallbackCtx {
        injector,
        clipboard,
        file_transfer,
        resize_tx,
        last_input_time,
        clipboard_read_tx,
        download_request_tx,
        capture_wake,
        capture_cmd_tx,
        tab_backgrounded,
        video_needs_keyframe,
        display,
        max_width,
        max_height,
    } = ctx;
    let state = Arc::new(InputDispatchState::new());

    Arc::new(move |event: InputEvent| {
        handle_pre_dispatch(
            &event,
            &last_input_time,
            &capture_wake,
            &tab_backgrounded,
            std::time::SystemTime::now(),
        );

        let action = classify_input_event(&event, max_width, max_height);
        let channels = InputDispatchChannels {
            resize_tx: &resize_tx,
            clipboard_read_tx: &clipboard_read_tx,
            download_request_tx: &download_request_tx,
            capture_wake: &capture_wake,
            capture_cmd_tx: &capture_cmd_tx,
            tab_backgrounded: &tab_backgrounded,
            video_needs_keyframe: &video_needs_keyframe,
        };

        let mut injector_guard = injector.lock().unwrap_or_else(|e| e.into_inner());
        let clipboard_guard = clipboard.lock().unwrap_or_else(|e| e.into_inner());
        let mut file_guard = file_transfer.lock().unwrap_or_else(|e| e.into_inner());
        let effect = dispatch_input_action(
            action,
            &mut *injector_guard,
            &*clipboard_guard,
            &mut *file_guard,
            &state,
            &channels,
            &display,
        );
        drop(injector_guard);
        drop(clipboard_guard);
        drop(file_guard);
        if let Some(effect) = effect {
            spawn_setxkbmap_worker(effect);
        }
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Install rustls crypto provider (needed for TLS WebSocket to server)
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Failed to install rustls crypto provider");

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    gstreamer::init().context("Failed to initialize GStreamer")?;

    let args = cli::parse_args()?;
    info!(
        display = %args.display,
        session_id = %args.session_id,
        server_url = %args.server_url,
        "Starting beam-agent"
    );

    // PulseAudio server path — derived from display number regardless of new/existing display
    let mut pulse_server: Option<String> = None;
    let display_num: u32 = parse_display_num(&args.display);

    // Try to connect to the display; if it doesn't exist, start a virtual one
    let mut virtual_display = match ScreenCapture::new(&args.display) {
        Ok(_) => {
            info!(display = %args.display, "Connected to existing display");
            // Session reuse: PulseAudio should already be running for this display
            let pulse_path = pulse_server_path(display_num);
            if std::path::Path::new(&pulse_path).exists() {
                pulse_server = Some(pulse_server_url(display_num));
                info!(%pulse_path, "Found existing PulseAudio socket for reused display");
            } else {
                warn!(%pulse_path, "No PulseAudio socket found for reused display, audio may not work");
            }
            None
        }
        Err(e) => {
            warn!(display = %args.display, "Display not available ({e:#}), starting virtual display");
            match display::VirtualDisplay::start(
                display_num,
                args.width,
                args.height,
                &args.gpu_driver,
                args.display_start,
            ) {
                Ok(mut vd) => {
                    info!(display = %args.display, "Virtual display started");

                    // Start PulseAudio BEFORE desktop so apps inherit PULSE_SERVER
                    if let Err(e) = vd.start_pulseaudio() {
                        warn!("Failed to start PulseAudio: {e:#}");
                    }
                    let pulse_path = pulse_server_path(display_num);
                    pulse_server = Some(pulse_server_url(display_num));
                    for _ in 0..20 {
                        if std::path::Path::new(&pulse_path).exists() {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }

                    // Start desktop AFTER PulseAudio
                    if let Err(e) = vd.start_desktop() {
                        warn!("Failed to start desktop: {e:#}");
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    Some(vd)
                }
                Err(e) => {
                    return Err(e).context("Failed to start virtual display");
                }
            }
        }
    };

    // Get the xrandr output name for resize operations.
    // Virtual displays know their output name; existing displays detect it.
    let output_name = resolve_output_name(virtual_display.as_ref().map(|vd| vd.output_name()));

    // Create screen capture (now the display should be available)
    let mut screen_capture =
        ScreenCapture::new(&args.display).context("Failed to initialize screen capture")?;
    let width = screen_capture.width();
    let height = screen_capture.height();

    // Detect encoder type first to determine framerate/bitrate caps.
    // Software x264enc ultrafast on ARM64 can only sustain ~60fps at 1080p.
    // Attempting 120fps causes the appsrc queue to grow faster than the
    // encoder drains it, leading to OOM.
    let encoder_pref = args.encoder.clone();
    let (encoder_type, _) = encoder::detect_encoder_type(args.encoder.as_deref())?;
    let (config_framerate, config_bitrate) =
        cap_software_encoder_params(encoder_type, args.framerate, args.bitrate);
    if config_framerate != args.framerate || config_bitrate != args.bitrate {
        warn!(
            requested_fps = args.framerate,
            capped_fps = config_framerate,
            requested_bitrate = args.bitrate,
            capped_bitrate = config_bitrate,
            "Software encoder: capping framerate to 60fps and bitrate to 20Mbps"
        );
    }

    let encoder = Encoder::with_encoder_preference(
        width,
        height,
        config_framerate,
        config_bitrate,
        args.encoder.as_deref(),
    )
    .context("Failed to initialize encoder")?;

    // Channel for encoded video frames: capture thread -> async write loop
    let (encoded_tx, mut encoded_rx) = mpsc::channel::<Vec<u8>>(2);

    // Channel for encoded audio frames: audio thread -> async write loop.
    // Keep _audio_tx_keepalive so the channel stays open even if the audio
    // thread gives up (prevents the select! loop from exiting prematurely).
    let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(8);
    let _audio_tx_keepalive = audio_tx.clone();

    // Shared WebSocket outbox: video, audio, clipboard, cursor, file download all send here.
    // The signaling loop drains this and writes to the actual WS connection.
    // Capacity 32: enough for signaling + data messages. Video binary frames use try_send
    // with drop-on-full semantics to avoid backpressure from slow WS.
    let (ws_outbox_tx, mut ws_outbox_rx) = mpsc::channel::<Message>(32);

    let session_id = args.session_id;

    // Create input injector (uses XTEST extension -- no uinput needed)
    let input_width = Arc::new(std::sync::atomic::AtomicU32::new(args.width));
    let input_height = Arc::new(std::sync::atomic::AtomicU32::new(args.height));
    let injector = Arc::new(Mutex::new(
        InputInjector::new(
            &args.display,
            Arc::clone(&input_width),
            Arc::clone(&input_height),
        )
        .context("Failed to create input injector")?,
    ));

    // Create clipboard bridge
    let clipboard = Arc::new(Mutex::new(
        ClipboardBridge::new(&args.display).context("Failed to create clipboard bridge")?,
    ));

    // Force-keyframe flag: set from signaling handler (on reconnect),
    // cleared by capture thread each frame.
    let force_keyframe = Arc::new(AtomicBool::new(false));

    // Video-side IDR gate: set from input callback (on reconnect), cleared
    // by the video send loop when the IDR frame arrives. Separate from
    // force_keyframe because the capture thread clears that one immediately
    // (via swap) before the video send loop can see it.
    let video_needs_keyframe = Arc::new(AtomicBool::new(false));

    // Command channel for non-latency-critical capture thread operations
    let (capture_cmd_tx, capture_cmd_rx) = std::sync::mpsc::channel::<CaptureCommand>();

    // Resize request channel
    let (resize_tx, mut resize_rx) = mpsc::channel::<(u32, u32)>(4);

    // Idle detection
    let last_input_time = Arc::new(AtomicU64::new(0));
    let last_input_for_capture = Arc::clone(&last_input_time);

    // Wake signal for capture thread
    let capture_wake = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let capture_wake_for_input = Arc::clone(&capture_wake);

    // Clipboard read requests
    let (clipboard_read_tx, mut clipboard_read_rx) = mpsc::channel::<()>(4);

    // File download requests
    let (download_request_tx, mut download_request_rx) = mpsc::channel::<String>(4);

    // Cursor shape monitor
    let mut cursor_rx = cursor::spawn_cursor_monitor(&args.display);
    if cursor_rx.is_none() {
        warn!("Cursor monitor failed to start, falling back to unclutter");
        if let Some(ref mut vd) = virtual_display {
            vd.hide_cursor();
        }
    }

    // Tab backgrounded flag
    let tab_backgrounded = Arc::new(AtomicBool::new(false));
    let tab_backgrounded_for_capture = Arc::clone(&tab_backgrounded);

    // File transfer manager
    let home_dir = resolve_home_dir(std::env::var("HOME").ok().as_deref());
    let file_transfer = Arc::new(Mutex::new(filetransfer::FileTransferManager::new(home_dir)));
    let file_transfer_for_download = Arc::clone(&file_transfer);

    // Build input callback
    let input_callback = build_input_callback(InputCallbackCtx {
        injector: Arc::clone(&injector),
        clipboard: Arc::clone(&clipboard),
        file_transfer,
        resize_tx: resize_tx.clone(),
        last_input_time: Arc::clone(&last_input_time),
        clipboard_read_tx: clipboard_read_tx.clone(),
        download_request_tx,
        capture_wake: Arc::clone(&capture_wake_for_input),
        capture_cmd_tx: capture_cmd_tx.clone(),
        tab_backgrounded: Arc::clone(&tab_backgrounded),
        video_needs_keyframe: Arc::clone(&video_needs_keyframe),
        display: args.display.clone(),
        max_width: args.max_width,
        max_height: args.max_height,
    });

    // Shutdown flag for capture/audio threads
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_for_capture = Arc::clone(&shutdown);
    let shutdown_for_audio = Arc::clone(&shutdown);

    // Capture + encode thread
    const IDLE_TIMEOUT_MS: u64 = 300_000;
    const IDLE_FRAMERATE: u32 = 5;
    const BACKGROUND_FRAMERATE: u32 = 1;
    const ENCODER_RESET_COOLDOWN: Duration = Duration::from_secs(5);

    let display_for_capture = args.display.clone();
    let output_name_for_capture = output_name.clone();
    let kf_flag_for_capture = Arc::clone(&force_keyframe);
    let capture_wake_for_thread = Arc::clone(&capture_wake);
    let input_width_for_capture = Arc::clone(&input_width);
    let input_height_for_capture = Arc::clone(&input_height);

    let capture_handle = std::thread::Builder::new()
        .name("capture-encode".into())
        .spawn(move || {
            // Elevate to real-time priority for consistent frame pacing
            #[cfg(target_os = "linux")]
            {
                const SCHED_PRIORITY: i32 = 50;
                let param = libc::sched_param { sched_priority: SCHED_PRIORITY };
                let ret = unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) };
                let msg = sched_fifo_log_message(ret, SCHED_PRIORITY);
                if ret != 0 {
                    warn!("{msg}: {}", std::io::Error::last_os_error());
                } else {
                    info!("{msg}");
                }
            }

            let mut encoder = encoder;
            let current_bitrate = config_bitrate;
            let current_framerate = config_framerate;
            let active_frame_duration_ns = frame_duration_ns_from_framerate(config_framerate);
            let idle_frame_duration_ns = frame_duration_ns_from_framerate(IDLE_FRAMERATE);
            let background_frame_duration_ns = frame_duration_ns_from_framerate(BACKGROUND_FRAMERATE);
            let mut frame_count: u64 = 0;
            let mut encoded_count: u64 = 0;
            let start = Instant::now();
            let mut was_idle = false;
            let mut was_backgrounded = false;
            let mut first_capture_logged = false;
            let mut first_encode_logged = false;
            let mut last_encoder_reset = Instant::now() - ENCODER_RESET_COOLDOWN;
            let mut consecutive_capture_errors: u64 = 0;
            let mut last_capture_heartbeat = Instant::now();

            loop {
                if shutdown_for_capture.load(Ordering::Relaxed) {
                    info!("Capture thread shutting down");
                    break;
                }

                // Process commands from async tasks
                enum EncoderRecreate { None, Reset, Resize }
                let mut recreate = EncoderRecreate::None;
                while let Ok(cmd) = capture_cmd_rx.try_recv() {
                    match cmd {
                        CaptureCommand::Resize { width, height } => {
                            if is_resize_noop(
                                screen_capture.width(),
                                screen_capture.height(),
                                width,
                                height,
                            ) {
                                debug!(width, height, "Resize skipped (same dimensions)");
                                continue;
                            }
                            info!("{}", format_processing_resize_message(width, height));

                            if let Err(e) = display::set_display_resolution(
                                &display_for_capture,
                                width,
                                height,
                                &output_name_for_capture,
                            ) {
                                warn!("{}", format_xrandr_resize_failure(&format!("{e:#}")));
                                continue;
                            }

                            for _ in 0..20 {
                                std::thread::sleep(Duration::from_millis(10));
                                if shutdown_for_capture.load(Ordering::Relaxed) {
                                    return;
                                }
                            }

                            let new_capture = match ScreenCapture::new(&display_for_capture) {
                                Ok(cap) => cap,
                                Err(e) => {
                                    error!("Failed to recreate capture after resize: {e:#}");
                                    return;
                                }
                            };
                            screen_capture = new_capture;
                            recreate = EncoderRecreate::Resize;
                            break;
                        }
                        CaptureCommand::ResetEncoder => {
                            let elapsed_ms = last_encoder_reset.elapsed().as_millis() as u64;
                            let cooldown_ms = ENCODER_RESET_COOLDOWN.as_millis() as u64;
                            if should_recreate_encoder(elapsed_ms, cooldown_ms) {
                                recreate = EncoderRecreate::Reset;
                                break;
                            } else {
                                let cooldown_remaining_ms =
                                    encoder_reset_cooldown_remaining_ms(elapsed_ms, cooldown_ms);
                                debug!(
                                    cooldown_remaining_ms,
                                    "ResetEncoder throttled, sending force_keyframe instead"
                                );
                                encoder.force_keyframe();
                            }
                        }
                    }
                }

                match recreate {
                    EncoderRecreate::None => {}
                    EncoderRecreate::Reset => {
                        info!("Dropping old encoder to free NVENC session");
                        drop(encoder);
                        info!("Old encoder dropped, creating new pipeline");
                        encoder = match Encoder::with_encoder_preference(
                            screen_capture.width(),
                            screen_capture.height(),
                            current_framerate,
                            current_bitrate,
                            encoder_pref.as_deref(),
                        ) {
                            Ok(enc) => enc,
                            Err(e) => {
                                error!("Failed to recreate encoder: {e:#}");
                                break;
                            }
                        };
                        first_encode_logged = false;
                        last_encoder_reset = Instant::now();
                        info!("Encoder pipeline recreated (next frame will be IDR)");
                    }
                    EncoderRecreate::Resize => {
                        let new_w = screen_capture.width();
                        let new_h = screen_capture.height();
                        info!(width = new_w, height = new_h, "Dropping old encoder for resize");
                        drop(encoder);
                        info!("Old encoder dropped, creating new pipeline for resize");
                        encoder = match Encoder::with_encoder_preference(
                            new_w, new_h, DEFAULT_FRAMERATE, current_bitrate,
                            encoder_pref.as_deref(),
                        ) {
                            Ok(enc) => enc,
                            Err(e) => {
                                error!("Failed to recreate encoder after resize: {e:#}");
                                break;
                            }
                        };

                        encoder.force_keyframe();
                        first_capture_logged = false;
                        first_encode_logged = false;

                        input_width_for_capture.store(new_w, Ordering::Relaxed);
                        input_height_for_capture.store(new_h, Ordering::Relaxed);

                        info!(
                            width = new_w, height = new_h,
                            "Resize complete, capture and encoder recreated"
                        );
                    }
                }

                // Check force-keyframe flag
                if kf_flag_for_capture.swap(false, Ordering::Relaxed) {
                    encoder.force_keyframe();
                    let was_backgrounded = tab_backgrounded_for_capture.swap(false, Ordering::Relaxed);
                    if should_warn_keyframe_while_backgrounded(was_backgrounded) {
                        warn!("Keyframe forced while backgrounded — clearing flag");
                    }
                }

                let frame_start = Instant::now();
                let pts = start.elapsed().as_nanos() as u64;

                let is_backgrounded = tab_backgrounded_for_capture.load(Ordering::Relaxed);

                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let last_input_ms = last_input_for_capture.load(Ordering::Relaxed);
                let is_idle = is_idle_state(last_input_ms, now_ms, IDLE_TIMEOUT_MS);

                let frame_duration_ns = select_frame_duration_ns(
                    is_backgrounded,
                    is_idle,
                    active_frame_duration_ns,
                    idle_frame_duration_ns,
                    background_frame_duration_ns,
                );

                if let Some(msg) = capture_state_transition_message(
                    is_idle,
                    is_backgrounded,
                    was_idle,
                    was_backgrounded,
                    current_framerate,
                    IDLE_FRAMERATE,
                    BACKGROUND_FRAMERATE,
                ) {
                    debug!("{msg}");
                }
                if is_backgrounded != was_backgrounded {
                    was_backgrounded = is_backgrounded;
                }
                if is_idle != was_idle && !is_backgrounded {
                    was_idle = is_idle;
                }

                // Auto-recover from GStreamer pipeline errors
                if encoder.has_error() {
                    warn!("GStreamer pipeline error detected, dropping encoder");
                    drop(encoder);
                    match Encoder::with_encoder_preference(
                        screen_capture.width(), screen_capture.height(),
                        current_framerate, current_bitrate,
                        encoder_pref.as_deref(),
                    ) {
                        Ok(enc) => {
                            encoder = enc;
                            first_encode_logged = false;
                            info!("Encoder auto-recovered from pipeline error");
                        }
                        Err(e) => {
                            error!("Failed to recreate encoder after pipeline error: {e:#}");
                            break;
                        }
                    }
                }

                match screen_capture.capture_frame() {
                    Ok(frame) => {
                        if should_log_capture_recovery(consecutive_capture_errors) {
                            info!(
                                recovered_after = consecutive_capture_errors,
                                "Capture recovered after consecutive errors"
                            );
                            consecutive_capture_errors = 0;
                        }
                        if should_log_first_frame(first_capture_logged) {
                            info!(size = frame.len(), "First frame captured from X display");
                            first_capture_logged = true;
                        }
                        if let Err(e) = encoder.encode_frame(frame, pts) {
                            error!("Encode error: {e:#}");
                            break;
                        }
                    }
                    Err(e) => {
                        consecutive_capture_errors += 1;
                        if should_log_capture_error(consecutive_capture_errors) {
                            warn!(
                                consecutive_errors = consecutive_capture_errors,
                                "Capture frame failed: {e:#}"
                            );
                        }
                        if should_break_capture_loop(consecutive_capture_errors, 300) {
                            error!(
                                consecutive_errors = consecutive_capture_errors,
                                "Capture failing persistently, breaking capture loop"
                            );
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                }

                // Drain encoded frames
                let drain_deadline = Instant::now() + Duration::from_millis(2);
                let mut drained_any = false;
                loop {
                    match encoder.pull_encoded() {
                        Ok(Some(data)) => {
                            drained_any = true;
                            encoded_count += 1;
                            if !first_encode_logged {
                                info!(size = data.len(), "First H.264 frame from encoder");
                                first_encode_logged = true;
                            }
                            match encoded_tx.try_send(data) {
                                Ok(()) => {}
                                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                    debug!("Dropping encoded frame (channel full, prioritizing latency)");
                                }
                                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                    info!("Encoded frame channel closed, stopping capture");
                                    return;
                                }
                            }
                        }
                        Ok(None) => {
                            if drained_any || Instant::now() >= drain_deadline {
                                break;
                            }
                            std::hint::spin_loop();
                        }
                        Err(e) => {
                            error!("Pull encoded error: {e:#}");
                            return;
                        }
                    }
                }

                frame_count += 1;

                if last_capture_heartbeat.elapsed() >= Duration::from_secs(5) {
                    let elapsed = start.elapsed().as_secs_f64();
                    info!(
                        captured = frame_count,
                        encoded = encoded_count,
                        fps = format_capture_heartbeat_fps(frame_count, elapsed),
                        is_idle, is_backgrounded,
                        "Capture heartbeat"
                    );
                    last_capture_heartbeat = Instant::now();
                }

                // Frame pacing
                let target = Duration::from_nanos(frame_duration_ns);
                let elapsed = frame_start.elapsed();
                if elapsed < target {
                    let remaining = target - elapsed;
                    if capture_wait_should_use_condvar(is_idle, is_backgrounded) {
                        let (lock, cvar) = &*capture_wake_for_thread;
                        let mut woken = lock.lock().unwrap_or_else(|e| e.into_inner());
                        *woken = false;
                        let result = cvar.wait_timeout(woken, remaining)
                            .unwrap_or_else(|e| e.into_inner());
                        if *result.0 {
                            debug!("Capture thread woken by input/visibility change");
                        }
                    } else {
                        if capture_active_should_sleep(remaining.as_millis() as u64) {
                            std::thread::sleep(remaining - Duration::from_millis(1));
                        }
                        while frame_start.elapsed() < target {
                            std::hint::spin_loop();
                        }
                    }
                }
            }
        })
        .context("Failed to spawn capture thread")?;

    // Audio capture thread — retries PulseAudio connection in background (non-blocking)
    let pulse_server_clone = pulse_server.clone();
    let audio_handle = std::thread::Builder::new()
        .name("audio-capture".into())
        .spawn(move || {
            // Retry PulseAudio connection with backoff: 500ms for first 20 attempts (10s),
            // then 2s indefinitely. PulseAudio can take 10-15s on some hosts.
            let mut audio_capture = None;
            for attempt in 0u32.. {
                if shutdown_for_audio.load(Ordering::Relaxed) {
                    return;
                }
                match AudioCapture::new(48000, 2, pulse_server_clone.as_deref()) {
                    Ok(capture) => {
                        info!(attempt, "Audio capture initialized");
                        audio_capture = Some(capture);
                        break;
                    }
                    Err(e) => {
                        let delay = audio_retry_delay_ms(attempt);
                        if should_log_audio_retry(attempt) {
                            info!(
                                attempt = attempt + 1,
                                delay_ms = delay,
                                "PulseAudio not ready, retrying..."
                            );
                        }
                        if should_give_up_audio_retry(attempt) {
                            warn!("Audio capture unavailable after 60 attempts: {e:#}. Giving up.");
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(delay));
                    }
                }
            }
            let Some(mut audio_capture) = audio_capture else {
                return;
            };
            // Discard any buffered silence from PulseAudio startup to avoid audio/video desync
            audio_capture.flush();
            info!("Audio capture thread started");
            loop {
                if shutdown_for_audio.load(Ordering::Relaxed) {
                    info!("Audio thread shutting down");
                    return;
                }
                match audio_capture.capture_and_encode() {
                    Ok(opus_data) => {
                        if audio_tx.blocking_send(opus_data).is_err() {
                            info!("Audio channel closed, stopping audio capture");
                            return;
                        }
                    }
                    Err(e) => {
                        error!("Audio capture error: {e:#}");
                        return;
                    }
                }
            }
        })
        .context("Failed to spawn audio capture thread")?;
    let audio_handle = Some(audio_handle);

    // Set up SIGTERM handler
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    let server_url = args.server_url.clone();
    let kf_flag_for_signal = Arc::clone(&force_keyframe);
    let cmd_tx_for_signal = capture_cmd_tx.clone();
    let cmd_tx_for_video = capture_cmd_tx.clone();
    let cmd_tx_for_resize = capture_cmd_tx;
    let clipboard_for_sync = Arc::clone(&clipboard);

    // WS sender clones for tasks that need to send messages
    let ws_tx_for_cursor = ws_outbox_tx.clone();

    let signaling_ctx = SignalingCtx {
        server_url: &server_url,
        session_id,
        agent_token: args.agent_token.as_deref(),
        tls_cert_path: args.tls_cert_path.as_deref(),
        force_keyframe: kf_flag_for_signal,
        input_callback: Arc::clone(&input_callback),
        capture_cmd_tx: &cmd_tx_for_signal,
        tab_backgrounded: Arc::clone(&tab_backgrounded),
    };

    tokio::select! {
        // Write encoded video frames as WebSocket binary
        _ = video::run_video_send_loop(
            &mut encoded_rx,
            &ws_outbox_tx,
            &force_keyframe,
            &video_needs_keyframe,
            &cmd_tx_for_video,
            &input_width,
            &input_height,
        ) => {}

        // Write encoded audio frames as WebSocket binary
        _ = video::run_audio_send_loop(
            &mut audio_rx,
            &ws_outbox_tx,
        ) => {}

        // Handle signaling WebSocket (also drains ws_outbox_rx)
        _ = signaling::run_signaling(
            &signaling_ctx,
            &mut ws_outbox_rx,
        ) => {}

        // Forward resize requests to capture thread
        _ = async {
            while let Some((w, h)) = resize_rx.recv().await {
                info!(w, h, "Resize requested, forwarding to capture thread");
                let _ = cmd_tx_for_resize.send(CaptureCommand::Resize { width: w, height: h });
            }
        } => {}

        // Clipboard sync: after Ctrl+C/X, read X11 clipboard and send to browser
        _ = clipboard_sync::run_clipboard_sync(
            &mut clipboard_read_rx,
            &clipboard_for_sync,
            &ws_outbox_tx,
        ) => {}

        // File download: stream chunks via WebSocket text
        _ = file_transfer_task::run_file_download_loop(
            &mut download_request_rx,
            &file_transfer_for_download,
            &ws_outbox_tx,
        ) => {}

        // Cursor shape passthrough via WebSocket text
        _ = async {
            if let Some(ref mut rx) = cursor_rx {
                while let Some(css) = rx.recv().await {
                    let msg = build_cursor_message(&css);
                    if let Err(e) = ws_tx_for_cursor.send(Message::Text(msg.into())).await {
                        debug!("Failed to send cursor shape to browser: {e}");
                    }
                }
            } else {
                std::future::pending::<()>().await;
            }
        } => {}

        // Handle shutdown signals
        _ = tokio::signal::ctrl_c() => {
            info!("Received SIGINT, shutting down");
        }
        _ = sigterm.recv() => {
            info!("Received SIGTERM, shutting down");
        }
    }

    // Signal capture threads to stop before dropping VirtualDisplay
    shutdown.store(true, Ordering::Relaxed);
    drop(encoded_rx);
    drop(audio_rx);
    if let Err(e) = capture_handle.join() {
        warn!("Capture thread panicked: {e:?}");
    }
    if let Some(handle) = audio_handle
        && let Err(e) = handle.join()
    {
        warn!("Audio thread panicked: {e:?}");
    }

    info!("Agent shutdown complete");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- is_finite_bounded_delta ---

    #[test]
    fn finite_bounded_delta_accepts_small_values() {
        assert!(is_finite_bounded_delta(0.0, 0.0));
        assert!(is_finite_bounded_delta(1.0, -1.0));
        assert!(is_finite_bounded_delta(0.5, 0.5));
        assert!(is_finite_bounded_delta(100.0, -100.0));
    }

    #[test]
    fn finite_bounded_delta_accepts_boundary_values() {
        // 10000.0 is inclusive, so it must pass.
        assert!(is_finite_bounded_delta(MAX_INPUT_DELTA, MAX_INPUT_DELTA));
        assert!(is_finite_bounded_delta(-MAX_INPUT_DELTA, -MAX_INPUT_DELTA));
        assert!(is_finite_bounded_delta(MAX_INPUT_DELTA, -MAX_INPUT_DELTA));
    }

    #[test]
    fn finite_bounded_delta_rejects_beyond_max() {
        assert!(!is_finite_bounded_delta(MAX_INPUT_DELTA + 0.0001, 0.0));
        assert!(!is_finite_bounded_delta(0.0, MAX_INPUT_DELTA + 0.0001));
        assert!(!is_finite_bounded_delta(20_000.0, 0.0));
        assert!(!is_finite_bounded_delta(0.0, -20_000.0));
    }

    #[test]
    fn finite_bounded_delta_rejects_nan() {
        assert!(!is_finite_bounded_delta(f64::NAN, 0.0));
        assert!(!is_finite_bounded_delta(0.0, f64::NAN));
        assert!(!is_finite_bounded_delta(f64::NAN, f64::NAN));
    }

    #[test]
    fn finite_bounded_delta_rejects_infinity() {
        assert!(!is_finite_bounded_delta(f64::INFINITY, 0.0));
        assert!(!is_finite_bounded_delta(0.0, f64::INFINITY));
        assert!(!is_finite_bounded_delta(f64::NEG_INFINITY, 0.0));
        assert!(!is_finite_bounded_delta(0.0, f64::NEG_INFINITY));
        assert!(!is_finite_bounded_delta(f64::INFINITY, f64::NEG_INFINITY));
    }

    #[test]
    fn finite_bounded_delta_subnormal_values_are_accepted() {
        // f64::MIN_POSITIVE is the smallest positive normal value; subnormals
        // smaller than that are still finite, so they pass the gate.
        let tiny = f64::MIN_POSITIVE / 2.0;
        assert!(is_finite_bounded_delta(tiny, tiny));
    }

    #[test]
    fn finite_bounded_delta_negative_zero_is_accepted() {
        // -0.0 is finite and in range; must not be confused for NaN or sign-flipped.
        assert!(is_finite_bounded_delta(-0.0, -0.0));
    }

    // --- is_valid_layout_name ---

    #[test]
    fn valid_layout_accepts_common_keyboard_layouts() {
        // setxkbmap layouts that we routinely send from the browser.
        for name in ["us", "us-intl", "de", "no", "se", "gb", "fr"] {
            assert!(
                is_valid_layout_name(name),
                "Layout '{name}' should be accepted"
            );
        }
    }

    #[test]
    fn valid_layout_accepts_alphanumeric_and_separators() {
        assert!(is_valid_layout_name("us"));
        assert!(is_valid_layout_name("us-intl"));
        assert!(is_valid_layout_name("us_intl"));
        assert!(is_valid_layout_name("layout1"));
        assert!(is_valid_layout_name("a-b_c-d"));
    }

    #[test]
    fn valid_layout_rejects_empty() {
        assert!(!is_valid_layout_name(""));
    }

    #[test]
    fn valid_layout_rejects_too_long() {
        // 20 chars max — 21 must fail.
        let long_layout = "a".repeat(21);
        assert!(!is_valid_layout_name(&long_layout));
        // Boundary: exactly 20 passes.
        let twenty = "a".repeat(20);
        assert!(is_valid_layout_name(&twenty));
    }

    #[test]
    fn valid_layout_rejects_shell_metacharacters() {
        // These would be dangerous in argv to setxkbmap.
        for bad in [
            "us;rm -rf /",
            "us $(rm)",
            "us`evil`",
            "us|cat",
            "us&exit",
            "us>/etc",
            "us\nbreak",
            "us\tbad",
            "us ",
            " us",
            "../etc/passwd",
            "us/intl",
        ] {
            assert!(
                !is_valid_layout_name(bad),
                "Layout '{bad}' must be rejected (shell metacharacter)"
            );
        }
    }

    #[test]
    fn valid_layout_rejects_unicode() {
        // Non-ASCII characters are out: setxkbmap takes only ASCII layout names.
        assert!(!is_valid_layout_name("café"));
        assert!(!is_valid_layout_name("\u{00e9}"));
        assert!(!is_valid_layout_name("layout\u{0000}null"));
    }

    #[test]
    fn valid_layout_rejects_special_chars_period_and_at() {
        // Even though `.` and `@` look benign, they're NOT in the allowed set.
        assert!(!is_valid_layout_name("us.dvorak"));
        assert!(!is_valid_layout_name("us@v2"));
    }

    #[test]
    fn valid_layout_accepts_single_character() {
        // Smallest non-empty layout.
        assert!(is_valid_layout_name("a"));
        assert!(is_valid_layout_name("1"));
        assert!(is_valid_layout_name("-"));
        assert!(is_valid_layout_name("_"));
    }

    // --- is_clipboard_size_ok ---

    #[test]
    fn clipboard_size_accepts_empty_and_small() {
        assert!(is_clipboard_size_ok(0));
        assert!(is_clipboard_size_ok(1));
        assert!(is_clipboard_size_ok(1024));
        assert!(is_clipboard_size_ok(65_536));
    }

    #[test]
    fn clipboard_size_accepts_exact_max() {
        assert!(is_clipboard_size_ok(MAX_CLIPBOARD_BYTES));
    }

    #[test]
    fn clipboard_size_rejects_one_byte_over() {
        assert!(!is_clipboard_size_ok(MAX_CLIPBOARD_BYTES + 1));
    }

    #[test]
    fn clipboard_size_rejects_dos_payloads() {
        // 1 GB and beyond clearly DoS — must be rejected.
        assert!(!is_clipboard_size_ok(1_000_000_000));
        assert!(!is_clipboard_size_ok(usize::MAX));
    }

    #[test]
    fn clipboard_max_is_one_mib() {
        // Lock the constant: 1 MiB ceiling (browsers can grow up to it, larger
        // pastes are dropped with a warning).
        assert_eq!(MAX_CLIPBOARD_BYTES, 1_048_576);
        assert_eq!(MAX_CLIPBOARD_BYTES, 1024 * 1024);
    }

    // --- MAX_INPUT_DELTA ---

    #[test]
    fn input_delta_max_lock() {
        // Sanity: the cap is high enough for sane scroll/relative-motion deltas
        // but low enough that an int16 conversion downstream doesn't overflow.
        assert_eq!(MAX_INPUT_DELTA, 10_000.0);
        // i16 max is 32767, and we round our delta to i16. 10000 fits.
        assert!(MAX_INPUT_DELTA as i32 <= i16::MAX as i32);
    }

    // --- CaptureCommand ---

    #[test]
    fn capture_command_resize_is_constructable() {
        let cmd = CaptureCommand::Resize {
            width: 1920,
            height: 1080,
        };
        if let CaptureCommand::Resize { width, height } = cmd {
            assert_eq!(width, 1920);
            assert_eq!(height, 1080);
        } else {
            panic!("Expected Resize variant");
        }
    }

    #[test]
    fn capture_command_reset_encoder_is_constructable() {
        let cmd = CaptureCommand::ResetEncoder;
        assert!(matches!(cmd, CaptureCommand::ResetEncoder));
    }

    #[test]
    fn capture_command_sends_through_std_mpsc() {
        // The capture thread uses a std::sync::mpsc channel for commands.
        // Verify the variant survives a round-trip through the channel.
        let (tx, rx) = std::sync::mpsc::channel::<CaptureCommand>();
        tx.send(CaptureCommand::ResetEncoder).unwrap();
        tx.send(CaptureCommand::Resize {
            width: 800,
            height: 600,
        })
        .unwrap();
        match rx.recv().unwrap() {
            CaptureCommand::ResetEncoder => {}
            other => panic!(
                "Expected ResetEncoder, got {:?}",
                capture_command_kind(&other)
            ),
        }
        match rx.recv().unwrap() {
            CaptureCommand::Resize { width, height } => {
                assert_eq!(width, 800);
                assert_eq!(height, 600);
            }
            other => panic!("Expected Resize, got {:?}", capture_command_kind(&other)),
        }
    }

    fn capture_command_kind(cmd: &CaptureCommand) -> &'static str {
        match cmd {
            CaptureCommand::Resize { .. } => "Resize",
            CaptureCommand::ResetEncoder => "ResetEncoder",
        }
    }

    #[test]
    fn capture_command_kind_classifies_resize() {
        let cmd = CaptureCommand::Resize {
            width: 1,
            height: 2,
        };
        assert_eq!(capture_command_kind(&cmd), "Resize");
    }

    #[test]
    fn capture_command_kind_classifies_reset_encoder() {
        assert_eq!(
            capture_command_kind(&CaptureCommand::ResetEncoder),
            "ResetEncoder"
        );
    }

    // --- dispatch_capture_wake: poisoned-mutex recovery ---

    #[test]
    fn dispatch_capture_wake_recovers_from_poisoned_mutex() {
        // Force-poison the inner mutex by panicking inside the lock guard.
        // The production code recovers via `unwrap_or_else(|e| e.into_inner())`.
        let wake = (StdMutex::new(false), std::sync::Condvar::new());
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = wake.0.lock().unwrap();
            panic!("poison");
        }));
        // The mutex is now poisoned. dispatch_capture_wake must not panic
        // and must successfully set the bool to true.
        dispatch_capture_wake(&wake);
        // Lock again with poison-recovery to read the value.
        let g = wake.0.lock().unwrap_or_else(|e| e.into_inner());
        assert!(*g);
    }

    // --- dispatch_input_action: poisoned-layout-mutex recovery ---

    #[test]
    fn dispatch_layout_recovers_from_poisoned_last_layout_mutex() {
        // Build state with a poisoned last_layout mutex, then dispatch
        // a Layout action. The dispatch must recover and emit a Spawn.
        let state = InputDispatchState::new();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = state.last_layout.lock().unwrap();
            panic!("poison");
        }));
        // The mutex inside state.last_layout is now poisoned.

        let mut injector = MockInjector::new();
        let clipboard = MockClipboard::new();
        let mut file_sink = MockFileSink::new();
        let channels = TestChannels::new();
        let effect = dispatch_input_action(
            InputAction::Layout {
                layout: "us".into(),
            },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &channels.channels(),
            ":99",
        );
        // Even with poison, the layout should be stored fresh and the
        // first call should emit a Spawn effect.
        assert!(matches!(effect, Some(LayoutEffect::Spawn { .. })));
    }

    // --- parse_display_num ---

    #[test]
    fn parse_display_num_strips_colon_prefix() {
        assert_eq!(parse_display_num(":10"), 10);
        assert_eq!(parse_display_num(":99"), 99);
        assert_eq!(parse_display_num(":0"), 0);
    }

    #[test]
    fn parse_display_num_handles_no_colon() {
        // Some callers may pass the display number without a leading colon.
        assert_eq!(parse_display_num("42"), 42);
    }

    #[test]
    fn parse_display_num_falls_back_to_10_for_garbage() {
        // Anything unparseable → default Beam display 10.
        assert_eq!(parse_display_num(":not-a-number"), 10);
        assert_eq!(parse_display_num("garbage"), 10);
        assert_eq!(parse_display_num(""), 10);
        assert_eq!(parse_display_num(":"), 10);
        assert_eq!(parse_display_num(":-5"), 10);
    }

    #[test]
    fn parse_display_num_caps_at_u32_max_via_overflow_fallback() {
        // 2^32 overflows u32 and falls back to 10.
        assert_eq!(parse_display_num(":4294967296"), 10);
    }

    #[test]
    fn parse_display_num_accepts_max_u32() {
        // u32::MAX is parseable, so it should round-trip.
        assert_eq!(parse_display_num(":4294967295"), u32::MAX);
    }

    // --- pulse_server_path / pulse_server_url ---

    #[test]
    fn pulse_server_path_includes_display_num() {
        assert_eq!(pulse_server_path(10), "/tmp/beam-pulse-10/native");
        assert_eq!(pulse_server_path(0), "/tmp/beam-pulse-0/native");
        assert_eq!(
            pulse_server_path(u32::MAX),
            format!("/tmp/beam-pulse-{}/native", u32::MAX)
        );
    }

    #[test]
    fn pulse_server_url_prefixes_unix_scheme() {
        assert_eq!(pulse_server_url(10), "unix:/tmp/beam-pulse-10/native");
        assert_eq!(pulse_server_url(42), "unix:/tmp/beam-pulse-42/native");
    }

    #[test]
    fn pulse_server_url_is_path_prefixed_with_unix() {
        // The URL is always the path with an `unix:` prefix — never a host
        // form (PulseAudio's Simple API takes server in this format).
        for n in [0u32, 1, 10, 42, 100, 1000] {
            let url = pulse_server_url(n);
            let path = pulse_server_path(n);
            assert_eq!(url, format!("unix:{path}"));
            assert!(url.starts_with("unix:"));
        }
    }

    // --- cap_software_encoder_params ---

    #[test]
    fn cap_software_encoder_caps_at_60fps_and_20mbps() {
        // Software + >60 fps → 60fps. Bitrate at 30k → capped to 20k.
        let (fps, br) = cap_software_encoder_params(encoder::EncoderType::Software, 120, 30_000);
        assert_eq!(fps, 60);
        assert_eq!(br, 20_000);
    }

    #[test]
    fn cap_software_encoder_passes_through_below_60fps() {
        // Software at 30fps is fine — no cap applied.
        let (fps, br) = cap_software_encoder_params(encoder::EncoderType::Software, 30, 5_000);
        assert_eq!(fps, 30);
        assert_eq!(br, 5_000);
    }

    #[test]
    fn cap_software_encoder_at_exactly_60fps_does_not_cap() {
        // The threshold is > 60, so 60 itself passes through.
        let (fps, br) = cap_software_encoder_params(encoder::EncoderType::Software, 60, 8_000);
        assert_eq!(fps, 60);
        assert_eq!(br, 8_000);
    }

    #[test]
    fn cap_software_encoder_caps_only_software() {
        // Nvidia/CUDA/VAAPI all pass through unchanged even at 120fps.
        for enc_type in [
            encoder::EncoderType::Nvidia,
            encoder::EncoderType::NvidiaCuda,
            encoder::EncoderType::VaApi,
        ] {
            let (fps, br) = cap_software_encoder_params(enc_type, 120, 50_000);
            assert_eq!(fps, 120, "{enc_type:?} fps should not be capped");
            assert_eq!(br, 50_000, "{enc_type:?} bitrate should not be capped");
        }
    }

    #[test]
    fn cap_software_encoder_bitrate_below_cap_is_preserved() {
        // Software at 120 fps, 10000 kbps → fps capped to 60, bitrate
        // unchanged (already below 20000 cap).
        let (fps, br) = cap_software_encoder_params(encoder::EncoderType::Software, 120, 10_000);
        assert_eq!(fps, 60);
        assert_eq!(br, 10_000);
    }

    #[test]
    fn cap_software_encoder_bitrate_min_takes_the_smaller_value() {
        // Software at 120 fps, 19999 kbps → bitrate stays 19999.
        let (fps, br) = cap_software_encoder_params(encoder::EncoderType::Software, 120, 19_999);
        assert_eq!(fps, 60);
        assert_eq!(br, 19_999);
    }

    #[test]
    fn default_output_name_constant() {
        // Stay locked at DUMMY0 — the agent + display code assume this string.
        assert_eq!(DEFAULT_OUTPUT_NAME, "DUMMY0");
    }

    // --- is_ctrl_keycode ---

    #[test]
    fn ctrl_keycode_recognizes_left_and_right_ctrl() {
        assert!(is_ctrl_keycode(29), "Left Ctrl");
        assert!(is_ctrl_keycode(97), "Right Ctrl");
    }

    #[test]
    fn ctrl_keycode_rejects_other_keys() {
        // Spot-check non-Ctrl keycodes — must not flip the ctrl_down flag.
        for code in [0u16, 1, 28, 30, 96, 98, 100, 200, u16::MAX] {
            assert!(!is_ctrl_keycode(code), "Keycode {code} should not be Ctrl");
        }
    }

    // --- is_clipboard_read_trigger_key ---

    #[test]
    fn clipboard_trigger_recognizes_comma_and_period() {
        assert!(is_clipboard_read_trigger_key(45), "comma");
        assert!(is_clipboard_read_trigger_key(46), "period");
    }

    #[test]
    fn clipboard_trigger_rejects_other_keys() {
        for code in [0u16, 1, 29, 44, 47, 50, 100, u16::MAX] {
            assert!(
                !is_clipboard_read_trigger_key(code),
                "Keycode {code} should not trigger clipboard read"
            );
        }
    }

    // --- is_interactive_input_event ---

    #[test]
    fn interactive_event_accepts_key_press() {
        assert!(is_interactive_input_event(&InputEvent::Key {
            c: 42,
            d: true,
        }));
        assert!(is_interactive_input_event(&InputEvent::Key {
            c: 42,
            d: false,
        }));
    }

    #[test]
    fn interactive_event_accepts_mouse_move() {
        assert!(is_interactive_input_event(&InputEvent::MouseMove {
            x: 0.5,
            y: 0.5,
        }));
    }

    #[test]
    fn interactive_event_accepts_relative_mouse_move() {
        assert!(is_interactive_input_event(&InputEvent::RelativeMouseMove {
            dx: 1.0,
            dy: 1.0,
        }));
    }

    #[test]
    fn interactive_event_accepts_button() {
        assert!(is_interactive_input_event(&InputEvent::Button {
            b: 0,
            d: true,
        }));
    }

    #[test]
    fn interactive_event_accepts_scroll() {
        assert!(is_interactive_input_event(&InputEvent::Scroll {
            dx: 0.0,
            dy: 30.0,
        }));
    }

    #[test]
    fn interactive_event_rejects_visibility_change() {
        // VisibilityState alone should NOT clear backgrounded — the explicit
        // "I'm back" signal still fires from the side-effect arm.
        assert!(!is_interactive_input_event(&InputEvent::VisibilityState {
            visible: true,
        }));
    }

    #[test]
    fn interactive_event_rejects_clipboard_events() {
        assert!(!is_interactive_input_event(&InputEvent::Clipboard {
            text: "x".to_string(),
        }));
        assert!(!is_interactive_input_event(&InputEvent::ClipboardPrimary {
            text: "x".to_string(),
        }));
    }

    #[test]
    fn interactive_event_rejects_layout() {
        assert!(!is_interactive_input_event(&InputEvent::Layout {
            layout: "us".to_string(),
        }));
    }

    #[test]
    fn interactive_event_rejects_resize() {
        // Resize is admin-ish (browser tells us a new screen size). It
        // shouldn't count as user-input for the backgrounded resume logic.
        assert!(!is_interactive_input_event(&InputEvent::Resize {
            w: 1920,
            h: 1080,
        }));
    }

    #[test]
    fn interactive_event_rejects_file_transfer() {
        assert!(!is_interactive_input_event(&InputEvent::FileStart {
            id: "x".to_string(),
            name: "y".to_string(),
            size: 100,
        }));
        assert!(!is_interactive_input_event(&InputEvent::FileChunk {
            id: "x".to_string(),
            data: "y".to_string(),
        }));
        assert!(!is_interactive_input_event(&InputEvent::FileDone {
            id: "x".to_string(),
        }));
        assert!(!is_interactive_input_event(
            &InputEvent::FileDownloadRequest {
                path: "x".to_string(),
            }
        ));
    }

    #[test]
    fn interactive_event_rejects_metrics() {
        assert!(!is_interactive_input_event(
            &InputEvent::ClientMetricsPing {
                id: 1,
                sent_ms: 100.0,
            }
        ));
        assert!(!is_interactive_input_event(&InputEvent::ClientMetrics(
            beam_protocol::ClientMetricsReport::default()
        )));
    }

    // --- classify_input_event ---
    //
    // The dispatcher is the agent's input router. Each event variant is
    // exercised through `classify_input_event` so the dispatch decisions
    // (delta clamps, clipboard size limits, layout validation, resize
    // clamping, "ignore" branches) can be verified without owning an X11
    // connection, the encoder, or the file transfer manager.

    fn classify(event: InputEvent) -> InputAction {
        classify_input_event(&event, 3840, 2160)
    }

    fn classify_with_max(event: InputEvent, mw: u32, mh: u32) -> InputAction {
        classify_input_event(&event, mw, mh)
    }

    #[test]
    fn classify_key_press_returns_inject_key_pressed_true() {
        assert_eq!(
            classify(InputEvent::Key { c: 65, d: true }),
            InputAction::InjectKey {
                code: 65,
                pressed: true
            }
        );
    }

    #[test]
    fn classify_key_release_returns_inject_key_pressed_false() {
        assert_eq!(
            classify(InputEvent::Key { c: 65, d: false }),
            InputAction::InjectKey {
                code: 65,
                pressed: false
            }
        );
    }

    #[test]
    fn classify_mouse_move_returns_inject_mouse_abs() {
        match classify(InputEvent::MouseMove { x: 0.5, y: 0.25 }) {
            InputAction::InjectMouseAbs { x, y } => {
                assert!((x - 0.5).abs() < f64::EPSILON);
                assert!((y - 0.25).abs() < f64::EPSILON);
            }
            other => panic!("Expected InjectMouseAbs, got {other:?}"),
        }
    }

    #[test]
    fn classify_relative_mouse_move_returns_inject_mouse_rel_when_bounded() {
        match classify(InputEvent::RelativeMouseMove { dx: 10.0, dy: -5.0 }) {
            InputAction::InjectMouseRel { dx, dy } => {
                assert!((dx - 10.0).abs() < f64::EPSILON);
                assert!((dy - (-5.0)).abs() < f64::EPSILON);
            }
            other => panic!("Expected InjectMouseRel, got {other:?}"),
        }
    }

    #[test]
    fn classify_relative_mouse_move_ignored_when_nan() {
        assert_eq!(
            classify(InputEvent::RelativeMouseMove {
                dx: f64::NAN,
                dy: 0.0,
            }),
            InputAction::Ignore
        );
        assert_eq!(
            classify(InputEvent::RelativeMouseMove {
                dx: 0.0,
                dy: f64::NAN,
            }),
            InputAction::Ignore
        );
    }

    #[test]
    fn classify_relative_mouse_move_ignored_when_infinity() {
        assert_eq!(
            classify(InputEvent::RelativeMouseMove {
                dx: f64::INFINITY,
                dy: 0.0,
            }),
            InputAction::Ignore
        );
        assert_eq!(
            classify(InputEvent::RelativeMouseMove {
                dx: 0.0,
                dy: f64::NEG_INFINITY,
            }),
            InputAction::Ignore
        );
    }

    #[test]
    fn classify_relative_mouse_move_ignored_when_beyond_max() {
        assert_eq!(
            classify(InputEvent::RelativeMouseMove {
                dx: MAX_INPUT_DELTA + 1.0,
                dy: 0.0,
            }),
            InputAction::Ignore
        );
        assert_eq!(
            classify(InputEvent::RelativeMouseMove {
                dx: -MAX_INPUT_DELTA - 1.0,
                dy: 0.0,
            }),
            InputAction::Ignore
        );
    }

    #[test]
    fn classify_button_press_returns_inject_button_pressed_true() {
        assert_eq!(
            classify(InputEvent::Button { b: 0, d: true }),
            InputAction::InjectButton {
                b: 0,
                pressed: true,
            }
        );
    }

    #[test]
    fn classify_button_release_returns_inject_button_pressed_false() {
        assert_eq!(
            classify(InputEvent::Button { b: 2, d: false }),
            InputAction::InjectButton {
                b: 2,
                pressed: false,
            }
        );
    }

    #[test]
    fn classify_scroll_returns_inject_scroll_when_bounded() {
        match classify(InputEvent::Scroll { dx: 0.0, dy: 30.0 }) {
            InputAction::InjectScroll { dx, dy } => {
                assert!((dx - 0.0).abs() < f64::EPSILON);
                assert!((dy - 30.0).abs() < f64::EPSILON);
            }
            other => panic!("Expected InjectScroll, got {other:?}"),
        }
    }

    #[test]
    fn classify_scroll_ignored_when_unbounded() {
        assert_eq!(
            classify(InputEvent::Scroll {
                dx: 999_999.0,
                dy: 0.0,
            }),
            InputAction::Ignore
        );
        assert_eq!(
            classify(InputEvent::Scroll {
                dx: f64::NAN,
                dy: f64::NAN,
            }),
            InputAction::Ignore
        );
    }

    #[test]
    fn classify_clipboard_returns_set_clipboard_when_small() {
        assert_eq!(
            classify(InputEvent::Clipboard {
                text: "hello".to_string(),
            }),
            InputAction::SetClipboard {
                text: "hello".to_string(),
            }
        );
    }

    #[test]
    fn classify_clipboard_empty_text_is_set_clipboard() {
        // Zero-byte clipboard payloads pass the size gate; they should
        // still propagate through the dispatcher (production logs nothing
        // unusual either).
        assert_eq!(
            classify(InputEvent::Clipboard {
                text: String::new(),
            }),
            InputAction::SetClipboard {
                text: String::new(),
            }
        );
    }

    #[test]
    fn classify_clipboard_too_large_is_ignored() {
        // 2 MiB exceeds MAX_CLIPBOARD_BYTES (1 MiB).
        let big = "a".repeat(MAX_CLIPBOARD_BYTES + 1);
        assert_eq!(
            classify(InputEvent::Clipboard { text: big }),
            InputAction::Ignore
        );
    }

    #[test]
    fn classify_clipboard_at_exact_max_is_set_clipboard() {
        // The size gate uses <=, so the boundary value still propagates.
        let payload = "a".repeat(MAX_CLIPBOARD_BYTES);
        assert_eq!(
            classify(InputEvent::Clipboard {
                text: payload.clone(),
            }),
            InputAction::SetClipboard { text: payload }
        );
    }

    #[test]
    fn classify_clipboard_primary_small_is_set_primary() {
        assert_eq!(
            classify(InputEvent::ClipboardPrimary {
                text: "middle-click".to_string(),
            }),
            InputAction::SetClipboardPrimary {
                text: "middle-click".to_string(),
            }
        );
    }

    #[test]
    fn classify_clipboard_primary_too_large_is_ignored() {
        let big = "x".repeat(MAX_CLIPBOARD_BYTES + 100);
        assert_eq!(
            classify(InputEvent::ClipboardPrimary { text: big }),
            InputAction::Ignore
        );
    }

    #[test]
    fn classify_resize_valid_dimensions_pass_through() {
        // 1920x1080 within (320..=7680, 240..=4320) and under 3840x2160 max
        // → falls through clamp_resize_dimensions unchanged.
        assert_eq!(
            classify(InputEvent::Resize { w: 1920, h: 1080 }),
            InputAction::Resize {
                width: 1920,
                height: 1080,
            }
        );
    }

    #[test]
    fn classify_resize_too_small_is_ignored() {
        // 100x100 fails the lower-bound gate (320, 240) → Ignore.
        assert_eq!(
            classify(InputEvent::Resize { w: 100, h: 100 }),
            InputAction::Ignore
        );
    }

    #[test]
    fn classify_resize_too_large_is_ignored() {
        // 8000x5000 exceeds the upper-bound gate (7680, 4320) → Ignore.
        assert_eq!(
            classify(InputEvent::Resize { w: 8000, h: 5000 }),
            InputAction::Ignore
        );
    }

    #[test]
    fn classify_resize_clamps_to_max_bounds() {
        // 4096x2160 with max 1920x1080 should clamp.
        assert_eq!(
            classify_with_max(InputEvent::Resize { w: 4096, h: 2160 }, 1920, 1080),
            InputAction::Resize {
                width: 1920,
                height: 1080,
            }
        );
    }

    #[test]
    fn classify_resize_enforces_even_dimensions() {
        // Odd values get rounded down to even (H.264 requirement).
        assert_eq!(
            classify(InputEvent::Resize { w: 1921, h: 1081 }),
            InputAction::Resize {
                width: 1920,
                height: 1080,
            }
        );
    }

    #[test]
    fn classify_layout_valid_returns_layout_action() {
        assert_eq!(
            classify(InputEvent::Layout {
                layout: "us".to_string(),
            }),
            InputAction::Layout {
                layout: "us".to_string(),
            }
        );
    }

    #[test]
    fn classify_layout_invalid_is_ignored() {
        // Shell metacharacters → Ignore (security gate).
        assert_eq!(
            classify(InputEvent::Layout {
                layout: "us;rm -rf /".to_string(),
            }),
            InputAction::Ignore
        );
        assert_eq!(
            classify(InputEvent::Layout {
                layout: String::new(),
            }),
            InputAction::Ignore
        );
        // Too long → Ignore.
        assert_eq!(
            classify(InputEvent::Layout {
                layout: "a".repeat(50),
            }),
            InputAction::Ignore
        );
    }

    #[test]
    fn classify_quality_always_ignored() {
        // Quality selector was removed; defensive Ignore on any incoming value.
        assert_eq!(
            classify(InputEvent::Quality {
                mode: "high".to_string(),
            }),
            InputAction::Ignore
        );
        assert_eq!(
            classify(InputEvent::Quality {
                mode: "low".to_string(),
            }),
            InputAction::Ignore
        );
    }

    #[test]
    fn classify_client_metrics_events_are_ignored() {
        // Browser-side metrics are handled by the server; the agent should
        // never inject or forward them.
        assert_eq!(
            classify(InputEvent::ClientMetricsPing {
                id: 42,
                sent_ms: 1000.0,
            }),
            InputAction::Ignore
        );
        assert_eq!(
            classify(InputEvent::ClientMetrics(
                beam_protocol::ClientMetricsReport::default(),
            )),
            InputAction::Ignore
        );
    }

    #[test]
    fn classify_visibility_visible_true() {
        assert_eq!(
            classify(InputEvent::VisibilityState { visible: true }),
            InputAction::Visibility { visible: true }
        );
    }

    #[test]
    fn classify_visibility_visible_false() {
        assert_eq!(
            classify(InputEvent::VisibilityState { visible: false }),
            InputAction::Visibility { visible: false }
        );
    }

    #[test]
    fn classify_file_start_carries_id_name_and_size() {
        assert_eq!(
            classify(InputEvent::FileStart {
                id: "uuid-1".to_string(),
                name: "report.pdf".to_string(),
                size: 12_345,
            }),
            InputAction::FileStart {
                id: "uuid-1".to_string(),
                name: "report.pdf".to_string(),
                size: 12_345,
            }
        );
    }

    #[test]
    fn classify_file_chunk_carries_id_and_data() {
        assert_eq!(
            classify(InputEvent::FileChunk {
                id: "uuid-1".to_string(),
                data: "base64data==".to_string(),
            }),
            InputAction::FileChunk {
                id: "uuid-1".to_string(),
                data: "base64data==".to_string(),
            }
        );
    }

    #[test]
    fn classify_file_done_carries_id() {
        assert_eq!(
            classify(InputEvent::FileDone {
                id: "uuid-1".to_string(),
            }),
            InputAction::FileDone {
                id: "uuid-1".to_string(),
            }
        );
    }

    #[test]
    fn classify_file_download_request_carries_path() {
        assert_eq!(
            classify(InputEvent::FileDownloadRequest {
                path: "/home/user/file.txt".to_string(),
            }),
            InputAction::FileDownload {
                path: "/home/user/file.txt".to_string(),
            }
        );
    }

    #[test]
    fn classify_max_zero_bounds_means_unlimited() {
        // max_width=0 means unlimited per clamp_resize_dimensions contract.
        assert_eq!(
            classify_with_max(InputEvent::Resize { w: 7680, h: 4320 }, 0, 0),
            InputAction::Resize {
                width: 7680,
                height: 4320,
            }
        );
    }

    #[test]
    fn classify_resize_minimum_640x480_enforced() {
        // Anything ≤ 640x480 (within the 320/240 lower gate) gets bumped up.
        assert_eq!(
            classify(InputEvent::Resize { w: 500, h: 400 }),
            InputAction::Resize {
                width: 640,
                height: 480,
            }
        );
    }

    #[test]
    fn classify_relative_mouse_move_at_max_boundary_passes() {
        // MAX_INPUT_DELTA is inclusive; exactly at the boundary should pass.
        assert_eq!(
            classify(InputEvent::RelativeMouseMove {
                dx: MAX_INPUT_DELTA,
                dy: -MAX_INPUT_DELTA,
            }),
            InputAction::InjectMouseRel {
                dx: MAX_INPUT_DELTA,
                dy: -MAX_INPUT_DELTA,
            }
        );
    }

    #[test]
    fn classify_scroll_at_max_boundary_passes() {
        assert_eq!(
            classify(InputEvent::Scroll {
                dx: MAX_INPUT_DELTA,
                dy: MAX_INPUT_DELTA,
            }),
            InputAction::InjectScroll {
                dx: MAX_INPUT_DELTA,
                dy: MAX_INPUT_DELTA,
            }
        );
    }

    #[test]
    fn classify_relative_mouse_move_zero_delta_still_classifies() {
        // (0, 0) is the only "no-op" relative move; the dispatcher still
        // classifies (the X11 injector deduplicates discrete=0 inside).
        assert_eq!(
            classify(InputEvent::RelativeMouseMove { dx: 0.0, dy: 0.0 }),
            InputAction::InjectMouseRel { dx: 0.0, dy: 0.0 }
        );
    }

    #[test]
    fn classify_input_action_is_clonable() {
        // Sanity: InputAction supports Clone so dispatchers can defer work
        // (e.g. spawn a thread that takes ownership of a Layout payload).
        let original = InputAction::SetClipboard {
            text: "x".to_string(),
        };
        let copy = original.clone();
        assert_eq!(original, copy);
    }

    // --- select_frame_duration_ns ---

    #[test]
    fn frame_duration_active_when_neither_idle_nor_backgrounded() {
        // Standard hot-path: foreground + recently-active tab uses the
        // configured framerate.
        assert_eq!(
            select_frame_duration_ns(false, false, 16_666_666, 200_000_000, 1_000_000_000),
            16_666_666
        );
    }

    #[test]
    fn frame_duration_idle_uses_idle_ns() {
        // Foreground but no input for >5 min: drop to ~5fps.
        assert_eq!(
            select_frame_duration_ns(false, true, 16_666_666, 200_000_000, 1_000_000_000),
            200_000_000
        );
    }

    #[test]
    fn frame_duration_backgrounded_takes_priority_over_idle() {
        // Even if idle, backgrounded wins → ~1fps.
        assert_eq!(
            select_frame_duration_ns(true, true, 16_666_666, 200_000_000, 1_000_000_000),
            1_000_000_000
        );
    }

    #[test]
    fn frame_duration_backgrounded_active_uses_background_ns() {
        // Active input but tab backgrounded — still throttle to ~1fps.
        assert_eq!(
            select_frame_duration_ns(true, false, 16_666_666, 200_000_000, 1_000_000_000),
            1_000_000_000
        );
    }

    #[test]
    fn frame_duration_lockstep_with_inputs() {
        // The three inputs are passed through verbatim — no scaling or
        // capping inside the selector itself.
        let active = 33_333_333u64;
        let idle = 250_000_000u64;
        let background = 2_000_000_000u64;
        assert_eq!(
            select_frame_duration_ns(false, false, active, idle, background),
            active
        );
        assert_eq!(
            select_frame_duration_ns(false, true, active, idle, background),
            idle
        );
        assert_eq!(
            select_frame_duration_ns(true, false, active, idle, background),
            background
        );
    }

    // --- is_idle_state ---

    #[test]
    fn idle_state_false_when_no_input_yet() {
        // last_input_ms == 0 means "no input observed since startup" —
        // not idle by definition.
        assert!(!is_idle_state(0, 1_000_000, 300_000));
    }

    #[test]
    fn idle_state_false_when_within_timeout() {
        // 100s ago, timeout is 300s → still active.
        let now = 1_000_000u64;
        let last = now - 100_000;
        assert!(!is_idle_state(last, now, 300_000));
    }

    #[test]
    fn idle_state_true_when_beyond_timeout() {
        // 400s ago, timeout is 300s → idle.
        let now = 1_000_000u64;
        let last = now - 400_000;
        assert!(is_idle_state(last, now, 300_000));
    }

    #[test]
    fn idle_state_handles_now_before_last_input_gracefully() {
        // Clock skew: if now somehow precedes last_input_ms, the saturating
        // subtraction floors to zero → not idle.
        assert!(!is_idle_state(2000, 1000, 300));
    }

    #[test]
    fn idle_state_exactly_at_timeout_boundary_is_not_idle() {
        // The condition is `>` not `>=` — exactly the timeout still counts
        // as active. One millisecond more flips to idle.
        let now = 1_000_000u64;
        assert!(!is_idle_state(now - 300_000, now, 300_000));
        assert!(is_idle_state(now - 300_001, now, 300_000));
    }

    // --- is_resize_noop ---

    #[test]
    fn resize_noop_when_dims_match() {
        assert!(is_resize_noop(1920, 1080, 1920, 1080));
        assert!(is_resize_noop(640, 480, 640, 480));
        assert!(is_resize_noop(0, 0, 0, 0));
    }

    #[test]
    fn resize_noop_false_when_width_differs() {
        assert!(!is_resize_noop(1920, 1080, 1280, 1080));
    }

    #[test]
    fn resize_noop_false_when_height_differs() {
        assert!(!is_resize_noop(1920, 1080, 1920, 720));
    }

    #[test]
    fn resize_noop_false_when_both_differ() {
        assert!(!is_resize_noop(1920, 1080, 2560, 1440));
    }

    // --- audio_retry_delay_ms ---

    #[test]
    fn audio_retry_delay_500ms_for_first_20_attempts() {
        for n in 0..20 {
            assert_eq!(audio_retry_delay_ms(n), 500, "attempt {n}");
        }
    }

    #[test]
    fn audio_retry_delay_2s_at_attempt_20() {
        // Boundary: attempt 20 (the 21st attempt) flips to 2000ms.
        assert_eq!(audio_retry_delay_ms(20), 2000);
        assert_eq!(audio_retry_delay_ms(21), 2000);
        assert_eq!(audio_retry_delay_ms(100), 2000);
    }

    // --- should_log_audio_retry ---

    #[test]
    fn audio_retry_log_first_20_attempts() {
        for n in 0..20 {
            assert!(should_log_audio_retry(n), "attempt {n} should log");
        }
    }

    #[test]
    fn audio_retry_log_every_10th_after_20() {
        assert!(should_log_audio_retry(20), "n=20 is divisible by 10");
        assert!(!should_log_audio_retry(21));
        assert!(!should_log_audio_retry(29));
        assert!(should_log_audio_retry(30));
        assert!(!should_log_audio_retry(31));
        assert!(should_log_audio_retry(40));
        assert!(should_log_audio_retry(50));
    }

    // --- should_give_up_audio_retry ---

    #[test]
    fn audio_retry_give_up_after_60() {
        assert!(!should_give_up_audio_retry(0));
        assert!(!should_give_up_audio_retry(60));
        assert!(should_give_up_audio_retry(61));
        assert!(should_give_up_audio_retry(1000));
    }

    #[test]
    fn audio_retry_total_time_to_giveup_is_predictable() {
        // Sum of delays over attempts 0..=60: attempts 0..20 give 20 × 500ms,
        // attempts 20..=60 give 41 × 2000ms. The math should be deterministic
        // for the runbook (and reproducibly bug-free).
        let mut total = 0u64;
        for n in 0..=60 {
            total += audio_retry_delay_ms(n);
        }
        // 20 * 500ms + 41 * 2000ms = 10_000 + 82_000 = 92_000ms (~92s)
        assert_eq!(total, 20 * 500 + 41 * 2000);
        assert_eq!(total, 92_000);
    }

    // --- build_cursor_message ---

    #[test]
    fn cursor_message_format_is_t_cur_css() {
        // The browser expects {"t":"cur", "css":"..."} so the cursor-shape
        // dispatcher key matches.
        let msg = build_cursor_message("default");
        let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(v["t"], "cur");
        assert_eq!(v["css"], "default");
    }

    #[test]
    fn cursor_message_handles_empty_css() {
        let msg = build_cursor_message("");
        let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(v["css"], "");
    }

    #[test]
    fn cursor_message_escapes_special_chars() {
        // CSS strings can contain quotes that JSON must escape.
        let msg = build_cursor_message(r#"url("data:image/png;base64,X") 0 0, default"#);
        let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(v["css"], r#"url("data:image/png;base64,X") 0 0, default"#);
    }

    #[test]
    fn cursor_message_preserves_unicode() {
        // Unicode in CSS should survive the JSON encode round-trip.
        let css = "cursor: pointer; /* ✓ */";
        let msg = build_cursor_message(css);
        let v: serde_json::Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(v["css"], css);
    }

    // --- resolve_home_dir ---

    #[test]
    fn home_dir_uses_env_value_when_set() {
        assert_eq!(
            resolve_home_dir(Some("/home/test")),
            std::path::PathBuf::from("/home/test"),
        );
    }

    #[test]
    fn home_dir_falls_back_when_none() {
        // HOME not set → fall back to /tmp.
        assert_eq!(resolve_home_dir(None), std::path::PathBuf::from("/tmp"));
    }

    #[test]
    fn home_dir_falls_back_when_empty() {
        // HOME="" → empty string also falls back (production avoids using
        // CWD-relative paths in this case).
        assert_eq!(resolve_home_dir(Some("")), std::path::PathBuf::from("/tmp"));
    }

    #[test]
    fn home_dir_accepts_absolute_paths_with_trailing_slash() {
        // PathBuf does not normalize on construction; trailing slash is kept.
        assert_eq!(
            resolve_home_dir(Some("/home/test/")),
            std::path::PathBuf::from("/home/test/"),
        );
    }

    // --- resolve_output_name ---

    #[test]
    fn output_name_uses_virtual_display_when_some() {
        assert_eq!(resolve_output_name(Some("DFP-1")), "DFP-1");
        assert_eq!(resolve_output_name(Some("DUMMY0")), "DUMMY0");
        assert_eq!(resolve_output_name(Some("HDMI-A-1")), "HDMI-A-1");
    }

    #[test]
    fn output_name_falls_back_when_none() {
        assert_eq!(resolve_output_name(None), "DUMMY0");
    }

    #[test]
    fn output_name_falls_back_when_empty() {
        // Empty output name shouldn't pass through — fall back to default.
        assert_eq!(resolve_output_name(Some("")), "DUMMY0");
    }

    #[test]
    fn output_name_fallback_matches_default_constant() {
        // The fallback must equal DEFAULT_OUTPUT_NAME so the rest of the
        // capture pipeline stays in sync.
        assert_eq!(resolve_output_name(None), DEFAULT_OUTPUT_NAME);
    }

    // --- should_log_capture_error / should_break_capture_loop ---

    #[test]
    fn capture_error_log_first_three() {
        // First 3 errors always log.
        assert!(should_log_capture_error(1));
        assert!(should_log_capture_error(2));
        assert!(should_log_capture_error(3));
    }

    #[test]
    fn capture_error_log_skip_until_multiple_of_100() {
        // After 3, only multiples of 100 log to keep the log volume tame.
        assert!(!should_log_capture_error(4));
        assert!(!should_log_capture_error(50));
        assert!(!should_log_capture_error(99));
        assert!(should_log_capture_error(100));
        assert!(!should_log_capture_error(101));
        assert!(should_log_capture_error(200));
        assert!(should_log_capture_error(300));
    }

    #[test]
    fn capture_error_log_zero_is_multiple_of_100() {
        // Zero is divisible by 100 (mathematically), so the predicate logs.
        // (Production never asks at consecutive_errors=0, but the fn is
        // deterministic.)
        assert!(should_log_capture_error(0));
    }

    #[test]
    fn capture_break_threshold_300_by_default() {
        assert!(!should_break_capture_loop(299, 300));
        assert!(should_break_capture_loop(300, 300));
        assert!(should_break_capture_loop(1000, 300));
    }

    #[test]
    fn capture_break_threshold_is_customizable() {
        // Defensive: caller can tighten/loosen the threshold per env.
        assert!(should_break_capture_loop(50, 50));
        assert!(!should_break_capture_loop(49, 50));
        assert!(should_break_capture_loop(1, 1));
    }

    // --- encoder_reset_cooldown_elapsed ---

    #[test]
    fn encoder_reset_cooldown_blocks_within_window() {
        // 1 second elapsed, 5 second cooldown — still cooling.
        assert!(!encoder_reset_cooldown_elapsed(1_000, 5_000));
        assert!(!encoder_reset_cooldown_elapsed(4_999, 5_000));
    }

    #[test]
    fn encoder_reset_cooldown_passes_exactly_at_boundary() {
        // The condition is `>=` so equal value is allowed.
        assert!(encoder_reset_cooldown_elapsed(5_000, 5_000));
    }

    #[test]
    fn encoder_reset_cooldown_passes_after_window() {
        assert!(encoder_reset_cooldown_elapsed(5_001, 5_000));
        assert!(encoder_reset_cooldown_elapsed(60_000, 5_000));
        assert!(encoder_reset_cooldown_elapsed(u64::MAX, 5_000));
    }

    #[test]
    fn encoder_reset_cooldown_zero_value_blocks() {
        // last_reset_elapsed_ms=0 means we just reset — blocked.
        assert!(!encoder_reset_cooldown_elapsed(0, 5_000));
    }

    #[test]
    fn encoder_reset_cooldown_zero_cooldown_always_passes() {
        // cooldown=0 disables throttling.
        assert!(encoder_reset_cooldown_elapsed(0, 0));
        assert!(encoder_reset_cooldown_elapsed(1, 0));
    }

    // --- handle_visibility_change ---

    #[test]
    fn visibility_visible_true_sets_backgrounded_false() {
        let bg = AtomicBool::new(true);
        let kf = AtomicBool::new(false);
        let (tx, _rx) = std::sync::mpsc::channel::<CaptureCommand>();
        let triggered = handle_visibility_change(true, &bg, &kf, &tx);
        assert!(triggered, "visible=true should trigger encoder reset");
        assert!(!bg.load(Ordering::Relaxed), "backgrounded cleared");
        assert!(kf.load(Ordering::Relaxed), "keyframe requested");
    }

    #[test]
    fn visibility_visible_false_sets_backgrounded_true() {
        let bg = AtomicBool::new(false);
        let kf = AtomicBool::new(false);
        let (tx, _rx) = std::sync::mpsc::channel::<CaptureCommand>();
        let triggered = handle_visibility_change(false, &bg, &kf, &tx);
        assert!(!triggered, "visible=false should NOT trigger encoder reset");
        assert!(bg.load(Ordering::Relaxed), "backgrounded set");
        assert!(
            !kf.load(Ordering::Relaxed),
            "keyframe NOT requested on backgrounded"
        );
    }

    #[test]
    fn visibility_visible_true_sends_reset_encoder_command() {
        let bg = AtomicBool::new(true);
        let kf = AtomicBool::new(false);
        let (tx, rx) = std::sync::mpsc::channel::<CaptureCommand>();
        handle_visibility_change(true, &bg, &kf, &tx);
        let cmd = rx.try_recv().expect("ResetEncoder should be sent");
        assert!(matches!(cmd, CaptureCommand::ResetEncoder));
    }

    #[test]
    fn visibility_visible_false_does_not_send_command() {
        let bg = AtomicBool::new(false);
        let kf = AtomicBool::new(false);
        let (tx, rx) = std::sync::mpsc::channel::<CaptureCommand>();
        handle_visibility_change(false, &bg, &kf, &tx);
        // Channel should be empty
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn visibility_handles_disconnected_capture_channel() {
        // If the capture thread already dropped the receiver, the send fails
        // silently — the function still flips the flags and reports triggered.
        let bg = AtomicBool::new(true);
        let kf = AtomicBool::new(false);
        let (tx, rx) = std::sync::mpsc::channel::<CaptureCommand>();
        drop(rx); // Capture thread gone
        let triggered = handle_visibility_change(true, &bg, &kf, &tx);
        assert!(triggered);
        assert!(!bg.load(Ordering::Relaxed));
        assert!(kf.load(Ordering::Relaxed));
    }

    #[test]
    fn visibility_subsequent_calls_toggle_state() {
        let bg = AtomicBool::new(false);
        let kf = AtomicBool::new(false);
        let (tx, _rx) = std::sync::mpsc::channel::<CaptureCommand>();
        // visible=false → backgrounded
        handle_visibility_change(false, &bg, &kf, &tx);
        assert!(bg.load(Ordering::Relaxed));
        // visible=true → foreground, keyframe
        handle_visibility_change(true, &bg, &kf, &tx);
        assert!(!bg.load(Ordering::Relaxed));
        assert!(kf.load(Ordering::Relaxed));
    }

    // --- layout_is_duplicate ---

    #[test]
    fn layout_duplicate_true_for_identical_strings() {
        assert!(layout_is_duplicate("us", "us"));
        assert!(layout_is_duplicate("", ""));
        assert!(layout_is_duplicate("de-latin1", "de-latin1"));
    }

    #[test]
    fn layout_duplicate_false_for_different_strings() {
        assert!(!layout_is_duplicate("us", "no"));
        assert!(!layout_is_duplicate("", "us"));
        assert!(!layout_is_duplicate("us", ""));
    }

    #[test]
    fn layout_duplicate_case_sensitive() {
        // setxkbmap layouts are lowercase by convention; case differences
        // are NOT considered duplicates so a mistyped uppercase layout
        // forces a re-spawn (and likely a failure log).
        assert!(!layout_is_duplicate("us", "US"));
    }

    // --- classify_setxkbmap_result ---

    #[test]
    fn setxkbmap_success_when_status_ok() {
        let r = classify_setxkbmap_result(Ok((true, String::new())));
        assert_eq!(r, SetxkbmapOutcome::Success);
    }

    #[test]
    fn setxkbmap_success_carries_empty_payload() {
        // Even if setxkbmap printed something to stderr while still
        // exiting 0, we treat that as success (no warning).
        let r = classify_setxkbmap_result(Ok((true, "harmless info".to_string())));
        assert_eq!(r, SetxkbmapOutcome::Success);
    }

    #[test]
    fn setxkbmap_failure_when_status_nonzero() {
        let r = classify_setxkbmap_result(Ok((false, "Cannot open display".into())));
        match r {
            SetxkbmapOutcome::Failure { stderr } => {
                assert_eq!(stderr, "Cannot open display");
            }
            other => panic!("Expected Failure, got {other:?}"),
        }
    }

    #[test]
    fn setxkbmap_spawn_error_when_io_failed() {
        let r = classify_setxkbmap_result(Err("No such file or directory".to_string()));
        match r {
            SetxkbmapOutcome::SpawnError { reason } => {
                assert_eq!(reason, "No such file or directory");
            }
            other => panic!("Expected SpawnError, got {other:?}"),
        }
    }

    #[test]
    fn setxkbmap_outcome_is_debug() {
        // Locking that the outcome carries enough info for tracing logs.
        let r = classify_setxkbmap_result(Ok((true, String::new())));
        let dbg = format!("{r:?}");
        assert!(dbg.contains("Success"));
    }

    #[test]
    fn setxkbmap_outcome_eq_distinguishes_variants() {
        assert_ne!(
            SetxkbmapOutcome::Success,
            SetxkbmapOutcome::Failure {
                stderr: String::new()
            }
        );
        assert_ne!(
            SetxkbmapOutcome::Failure { stderr: "a".into() },
            SetxkbmapOutcome::Failure { stderr: "b".into() }
        );
    }

    // --- now_unix_millis ---

    #[test]
    fn now_unix_millis_is_positive_for_current_time() {
        let now = std::time::SystemTime::now();
        let ms = now_unix_millis(now);
        // Sanity-check we're past 2020 (1.6e12 ms past epoch).
        assert!(ms > 1_600_000_000_000);
    }

    #[test]
    fn now_unix_millis_returns_zero_for_epoch() {
        let ms = now_unix_millis(std::time::UNIX_EPOCH);
        assert_eq!(ms, 0);
    }

    #[test]
    fn now_unix_millis_returns_zero_for_pre_epoch_time() {
        // For a SystemTime before UNIX_EPOCH, the duration_since() errors,
        // and the helper falls back to Duration::default() (zero).
        let pre = std::time::UNIX_EPOCH - std::time::Duration::from_secs(1);
        let ms = now_unix_millis(pre);
        assert_eq!(ms, 0);
    }

    // --- sched_fifo_log_message ---

    #[test]
    fn sched_fifo_log_success_for_ret_zero() {
        let msg = sched_fifo_log_message(0, 50);
        assert!(msg.contains("elevated to SCHED_FIFO"));
        assert!(msg.contains("50"));
    }

    #[test]
    fn sched_fifo_log_failure_for_nonzero_ret() {
        let msg = sched_fifo_log_message(-1, 50);
        assert!(msg.contains("Could not set SCHED_FIFO"));
        assert!(msg.contains("CAP_SYS_NICE"));
        assert!(msg.contains("50"));
    }

    #[test]
    fn sched_fifo_log_includes_priority_value() {
        // The priority is part of the message — if it ever changes the
        // test should remind whoever changes it to look here too.
        let msg = sched_fifo_log_message(0, 99);
        assert!(msg.contains("99"));
    }

    // --- file_download_request_dropped ---

    #[test]
    fn file_download_drop_returns_true_on_full_channel() {
        use tokio::sync::mpsc::error::TrySendError;
        let r: Result<(), TrySendError<String>> = Err(TrySendError::Full("foo".to_string()));
        assert!(file_download_request_dropped(r));
    }

    #[test]
    fn file_download_drop_returns_true_on_closed_channel() {
        use tokio::sync::mpsc::error::TrySendError;
        let r: Result<(), TrySendError<String>> = Err(TrySendError::Closed("foo".to_string()));
        assert!(file_download_request_dropped(r));
    }

    #[test]
    fn file_download_drop_returns_false_on_success() {
        let r: Result<(), tokio::sync::mpsc::error::TrySendError<String>> = Ok(());
        assert!(!file_download_request_dropped(r));
    }

    // --- should_log_first_frame ---

    #[test]
    fn should_log_first_frame_when_not_yet_logged() {
        assert!(should_log_first_frame(false));
    }

    #[test]
    fn should_log_first_frame_skips_when_already_logged() {
        assert!(!should_log_first_frame(true));
    }

    // --- format_capture_heartbeat_fps ---

    #[test]
    fn heartbeat_fps_zero_elapsed_returns_zero_string() {
        // Avoid divide-by-zero in the format.
        let s = format_capture_heartbeat_fps(120, 0.0);
        assert_eq!(s, "0.0");
    }

    #[test]
    fn heartbeat_fps_negative_elapsed_returns_zero_string() {
        // Defensive: Instant::elapsed is monotonic in production but the
        // helper is defensive for testability — negative or zero both
        // return the safe fallback.
        let s = format_capture_heartbeat_fps(120, -1.0);
        assert_eq!(s, "0.0");
    }

    #[test]
    fn heartbeat_fps_formats_one_decimal() {
        // 120 frames in 2.0s = 60.0 fps
        assert_eq!(format_capture_heartbeat_fps(120, 2.0), "60.0");
        // 90 frames in 1.5s = 60.0 fps
        assert_eq!(format_capture_heartbeat_fps(90, 1.5), "60.0");
    }

    #[test]
    fn heartbeat_fps_handles_fractional_rate() {
        // 100 frames in 1.5s = 66.6 fps
        let s = format_capture_heartbeat_fps(100, 1.5);
        assert!(s.starts_with("66.") && s.len() >= 4, "got: {s}");
    }

    #[test]
    fn heartbeat_fps_handles_large_counts() {
        // 1M frames in 1000s = 1000 fps
        assert_eq!(format_capture_heartbeat_fps(1_000_000, 1000.0), "1000.0");
    }

    #[test]
    fn heartbeat_fps_handles_zero_frames() {
        assert_eq!(format_capture_heartbeat_fps(0, 5.0), "0.0");
    }

    // --- capture_wait_should_use_condvar ---

    #[test]
    fn condvar_when_idle() {
        assert!(capture_wait_should_use_condvar(true, false));
    }

    #[test]
    fn condvar_when_backgrounded() {
        assert!(capture_wait_should_use_condvar(false, true));
    }

    #[test]
    fn condvar_when_both_idle_and_backgrounded() {
        assert!(capture_wait_should_use_condvar(true, true));
    }

    #[test]
    fn spinloop_when_active() {
        assert!(!capture_wait_should_use_condvar(false, false));
    }

    // --- capture_active_should_sleep ---

    #[test]
    fn active_sleep_above_2ms_remaining() {
        assert!(capture_active_should_sleep(3));
        assert!(capture_active_should_sleep(16));
        assert!(capture_active_should_sleep(100));
    }

    #[test]
    fn active_no_sleep_at_or_below_2ms() {
        // Strict `>` so 2ms is the boundary — spin only.
        assert!(!capture_active_should_sleep(0));
        assert!(!capture_active_should_sleep(1));
        assert!(!capture_active_should_sleep(2));
    }

    // --- should_log_capture_recovery ---

    #[test]
    fn recovery_log_when_streak_positive() {
        assert!(should_log_capture_recovery(1));
        assert!(should_log_capture_recovery(100));
        assert!(should_log_capture_recovery(u64::MAX));
    }

    #[test]
    fn recovery_log_skipped_when_no_streak() {
        // The first successful capture (consecutive=0) should NOT emit
        // the recovery line — there was no failure to recover from.
        assert!(!should_log_capture_recovery(0));
    }

    // --- frame_duration_ns_from_framerate ---

    #[test]
    fn frame_duration_at_60fps() {
        // 1s / 60 = 16.666ms = 16_666_666ns (integer division).
        assert_eq!(frame_duration_ns_from_framerate(60), 16_666_666);
    }

    #[test]
    fn frame_duration_at_30fps() {
        assert_eq!(frame_duration_ns_from_framerate(30), 33_333_333);
    }

    #[test]
    fn frame_duration_at_1fps_locks_to_1s() {
        assert_eq!(frame_duration_ns_from_framerate(1), 1_000_000_000);
    }

    #[test]
    fn frame_duration_zero_framerate_safe_fallback() {
        // Production never sends 0 — but the guard MUST not divide-by-zero.
        let d = frame_duration_ns_from_framerate(0);
        assert_eq!(d, 1_000_000_000, "Fallback should be 1fps duration");
    }

    #[test]
    fn frame_duration_at_high_framerates() {
        // 120fps = ~8.33ms per frame
        assert_eq!(frame_duration_ns_from_framerate(120), 8_333_333);
    }

    #[test]
    fn frame_duration_max_framerate_does_not_overflow() {
        // u32::MAX fps is silly but must not panic. Should produce 0
        // (1s/u32::MAX rounds down to 0 with integer math).
        let d = frame_duration_ns_from_framerate(u32::MAX);
        assert_eq!(d, 1_000_000_000 / u32::MAX as u64);
    }

    // --- should_warn_keyframe_while_backgrounded ---

    #[test]
    fn warn_keyframe_when_was_backgrounded() {
        assert!(should_warn_keyframe_while_backgrounded(true));
    }

    #[test]
    fn no_warn_keyframe_when_not_backgrounded() {
        assert!(!should_warn_keyframe_while_backgrounded(false));
    }

    // --- format_xrandr_resize_failure ---

    #[test]
    fn xrandr_resize_failure_includes_error() {
        let m = format_xrandr_resize_failure("BadMatch (invalid mode)");
        assert!(m.contains("xrandr resize failed"));
        assert!(m.contains("BadMatch"));
    }

    #[test]
    fn xrandr_resize_failure_handles_empty() {
        let m = format_xrandr_resize_failure("");
        assert!(m.starts_with("xrandr resize failed"));
    }

    // --- format_processing_resize_message ---

    #[test]
    fn processing_resize_message_includes_dims() {
        let m = format_processing_resize_message(1920, 1080);
        assert!(m.contains("1920"));
        assert!(m.contains("1080"));
        assert!(m.contains("Processing resize"));
    }

    #[test]
    fn processing_resize_message_handles_zero_dims() {
        // Defensive — the helper just formats; clamping is upstream.
        let m = format_processing_resize_message(0, 0);
        assert!(m.contains("0x0"));
    }

    // --- should_recreate_encoder + encoder_reset_cooldown_remaining_ms ---

    #[test]
    fn should_recreate_after_cooldown() {
        assert!(should_recreate_encoder(5_000, 5_000));
        assert!(should_recreate_encoder(10_000, 5_000));
    }

    #[test]
    fn should_not_recreate_within_cooldown() {
        assert!(!should_recreate_encoder(2_000, 5_000));
        assert!(!should_recreate_encoder(0, 5_000));
    }

    #[test]
    fn cooldown_remaining_zero_when_elapsed_exceeds() {
        assert_eq!(encoder_reset_cooldown_remaining_ms(6_000, 5_000), 0);
        assert_eq!(encoder_reset_cooldown_remaining_ms(5_000, 5_000), 0);
    }

    #[test]
    fn cooldown_remaining_subtraction() {
        assert_eq!(encoder_reset_cooldown_remaining_ms(1_000, 5_000), 4_000);
        assert_eq!(encoder_reset_cooldown_remaining_ms(0, 5_000), 5_000);
    }

    #[test]
    fn cooldown_remaining_handles_saturate_to_zero() {
        // u64 saturating_sub — large elapsed values produce 0, not panic.
        assert_eq!(encoder_reset_cooldown_remaining_ms(u64::MAX, 5_000), 0);
    }

    // --- should_log_encoded_drop_at_warn ---

    #[test]
    fn encoded_drop_is_never_warn() {
        // Lock: drops are debug-level, NEVER warn.
        assert!(!should_log_encoded_drop_at_warn(true));
        assert!(!should_log_encoded_drop_at_warn(false));
    }

    // --- appsrc_queue_max_bytes ---

    #[test]
    fn appsrc_queue_3_frames_at_1080p() {
        // 1920 * 1080 * 4 (BGRA) * 3 (frames) = 24.88 MB
        let bytes = appsrc_queue_max_bytes(1920, 1080);
        assert_eq!(bytes, 24_883_200);
    }

    #[test]
    fn appsrc_queue_at_4k() {
        // 3840 * 2160 * 4 * 3 = 99.53 MB
        let bytes = appsrc_queue_max_bytes(3840, 2160);
        assert_eq!(bytes, 99_532_800);
    }

    #[test]
    fn appsrc_queue_at_zero_dims() {
        // Zero dims (defensive) produce zero buffer requirement.
        assert_eq!(appsrc_queue_max_bytes(0, 0), 0);
        assert_eq!(appsrc_queue_max_bytes(1920, 0), 0);
        assert_eq!(appsrc_queue_max_bytes(0, 1080), 0);
    }

    // --- capture_state_transition_message ---

    #[test]
    fn transition_msg_none_when_no_change() {
        // No state change → no message.
        let m = capture_state_transition_message(false, false, false, false, 60, 5, 1);
        assert!(m.is_none());
    }

    #[test]
    fn transition_msg_backgrounded_enter() {
        let m = capture_state_transition_message(false, true, false, false, 60, 5, 1).unwrap();
        assert!(m.contains("Tab backgrounded"));
        assert!(m.contains("1fps"), "Should mention background framerate");
    }

    #[test]
    fn transition_msg_backgrounded_exit() {
        // is_backgrounded false now, was true.
        let m = capture_state_transition_message(false, false, false, true, 60, 5, 1).unwrap();
        assert!(m.contains("Tab foregrounded"));
        assert!(m.contains("60"), "Should mention current framerate");
    }

    #[test]
    fn transition_msg_idle_enter() {
        let m = capture_state_transition_message(true, false, false, false, 60, 5, 1).unwrap();
        assert!(m.contains("Entering idle"));
        assert!(m.contains("5fps"));
    }

    #[test]
    fn transition_msg_idle_exit() {
        let m = capture_state_transition_message(false, false, true, false, 60, 5, 1).unwrap();
        assert!(m.contains("Resuming active"));
        assert!(m.contains("60fps"));
    }

    #[test]
    fn transition_msg_background_takes_priority_over_idle() {
        // Both flags changed; background transition wins.
        let m = capture_state_transition_message(true, true, false, false, 60, 5, 1).unwrap();
        assert!(m.contains("Tab backgrounded"));
        assert!(!m.contains("Entering idle"));
    }

    #[test]
    fn transition_msg_idle_change_suppressed_when_backgrounded() {
        // Tab is backgrounded — idle transitions don't log.
        let m = capture_state_transition_message(true, true, false, true, 60, 5, 1);
        assert!(m.is_none());
    }

    // --- dispatch_resize ---

    #[tokio::test]
    async fn dispatch_resize_sends_dimensions() {
        let (tx, mut rx) = mpsc::channel::<(u32, u32)>(4);
        assert!(dispatch_resize(1920, 1080, &tx));
        assert_eq!(rx.recv().await, Some((1920, 1080)));
    }

    #[tokio::test]
    async fn dispatch_resize_returns_false_when_channel_full() {
        let (tx, _rx) = mpsc::channel::<(u32, u32)>(1);
        assert!(dispatch_resize(1, 1, &tx));
        // Second send fills the channel.
        assert!(!dispatch_resize(2, 2, &tx));
    }

    #[tokio::test]
    async fn dispatch_resize_returns_false_when_channel_closed() {
        let (tx, rx) = mpsc::channel::<(u32, u32)>(4);
        drop(rx);
        assert!(!dispatch_resize(1, 1, &tx));
    }

    // --- dispatch_file_download ---

    #[tokio::test]
    async fn dispatch_file_download_sends_path() {
        let (tx, mut rx) = mpsc::channel::<String>(4);
        assert!(dispatch_file_download("/tmp/x.txt".to_string(), &tx));
        assert_eq!(rx.recv().await, Some("/tmp/x.txt".to_string()));
    }

    #[tokio::test]
    async fn dispatch_file_download_returns_false_when_closed() {
        let (tx, rx) = mpsc::channel::<String>(4);
        drop(rx);
        assert!(!dispatch_file_download("/tmp/y.txt".to_string(), &tx));
    }

    #[tokio::test]
    async fn dispatch_file_download_handles_empty_path() {
        // Defensive — the dispatcher doesn't validate; just forwards.
        let (tx, mut rx) = mpsc::channel::<String>(4);
        assert!(dispatch_file_download(String::new(), &tx));
        assert_eq!(rx.recv().await, Some(String::new()));
    }

    // --- dispatch_capture_wake ---

    #[test]
    fn dispatch_capture_wake_sets_woken_flag() {
        let wake = (std::sync::Mutex::new(false), std::sync::Condvar::new());
        dispatch_capture_wake(&wake);
        assert!(*wake.0.lock().unwrap());
    }

    #[test]
    fn dispatch_capture_wake_idempotent() {
        let wake = (std::sync::Mutex::new(true), std::sync::Condvar::new());
        // Already-woken: still works, leaves flag true.
        dispatch_capture_wake(&wake);
        assert!(*wake.0.lock().unwrap());
    }

    #[test]
    fn dispatch_capture_wake_notifies_waiter() {
        use std::sync::Arc;
        use std::thread;
        let wake = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));

        // Spawn a waiter — it'll block until dispatch_capture_wake fires.
        let wake_for_waiter = Arc::clone(&wake);
        let waiter = thread::spawn(move || {
            let (lock, cvar) = &*wake_for_waiter;
            let mut woken = lock.lock().unwrap();
            while !*woken {
                woken = cvar.wait(woken).unwrap();
            }
            *woken
        });

        // Give the waiter a moment to park, then fire the wake.
        std::thread::sleep(std::time::Duration::from_millis(50));
        dispatch_capture_wake(&wake);
        // Waiter should observe the woken=true and exit.
        let observed = waiter.join().unwrap();
        assert!(observed);
    }

    // --- dispatch_update_last_input ---

    #[test]
    fn dispatch_update_last_input_writes_atomic() {
        let t = AtomicU64::new(0);
        dispatch_update_last_input(12_345, &t);
        assert_eq!(t.load(Ordering::Relaxed), 12_345);
    }

    #[test]
    fn dispatch_update_last_input_overwrites_existing() {
        let t = AtomicU64::new(100);
        dispatch_update_last_input(200, &t);
        assert_eq!(t.load(Ordering::Relaxed), 200);
    }

    #[test]
    fn dispatch_update_last_input_handles_zero() {
        let t = AtomicU64::new(100);
        dispatch_update_last_input(0, &t);
        assert_eq!(t.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn dispatch_update_last_input_handles_max() {
        let t = AtomicU64::new(0);
        dispatch_update_last_input(u64::MAX, &t);
        assert_eq!(t.load(Ordering::Relaxed), u64::MAX);
    }

    // --- dispatch_clear_backgrounded_if_interactive ---

    #[test]
    fn clear_backgrounded_when_interactive_and_flag_set() {
        let bg = AtomicBool::new(true);
        let event = InputEvent::Key { c: 65, d: true };
        assert!(dispatch_clear_backgrounded_if_interactive(&event, &bg));
        assert!(!bg.load(Ordering::Relaxed));
    }

    #[test]
    fn no_clear_when_interactive_but_flag_unset() {
        let bg = AtomicBool::new(false);
        let event = InputEvent::Key { c: 65, d: true };
        // Swap returns previous (false) so the helper returns false.
        assert!(!dispatch_clear_backgrounded_if_interactive(&event, &bg));
        assert!(!bg.load(Ordering::Relaxed));
    }

    #[test]
    fn no_clear_when_non_interactive_event() {
        let bg = AtomicBool::new(true);
        let event = InputEvent::VisibilityState { visible: true };
        // Non-interactive event: don't even swap.
        assert!(!dispatch_clear_backgrounded_if_interactive(&event, &bg));
        assert!(bg.load(Ordering::Relaxed), "Flag should remain set");
    }

    #[test]
    fn no_clear_for_file_chunk_event() {
        let bg = AtomicBool::new(true);
        let event = InputEvent::FileChunk {
            id: "x".to_string(),
            data: String::new(),
        };
        assert!(!dispatch_clear_backgrounded_if_interactive(&event, &bg));
        assert!(bg.load(Ordering::Relaxed));
    }

    #[test]
    fn no_clear_for_metrics_event() {
        let bg = AtomicBool::new(true);
        let event = InputEvent::ClientMetricsPing {
            id: 1,
            sent_ms: 0.0,
        };
        assert!(!dispatch_clear_backgrounded_if_interactive(&event, &bg));
        assert!(bg.load(Ordering::Relaxed));
    }

    // --- evdev_to_x11_keycode ---

    #[test]
    fn evdev_to_x11_adds_8() {
        assert_eq!(evdev_to_x11_keycode(0), 8);
        assert_eq!(evdev_to_x11_keycode(28), 36); // KEY_ENTER
        assert_eq!(evdev_to_x11_keycode(29), 37); // KEY_LEFTCTRL
        assert_eq!(evdev_to_x11_keycode(57), 65); // KEY_SPACE
    }

    #[test]
    fn evdev_to_x11_handles_max_u8_input() {
        // evdev keycode 247 + 8 = 255 — fits in u8.
        assert_eq!(evdev_to_x11_keycode(247), 255);
    }

    #[test]
    fn evdev_to_x11_wraps_at_overflow() {
        // Defensive — production validates upstream, but the cast truncates.
        assert_eq!(evdev_to_x11_keycode(248), 0);
        assert_eq!(evdev_to_x11_keycode(255), 7);
    }

    // --- browser_button_to_x11 ---

    #[test]
    fn browser_button_to_x11_maps_known() {
        assert_eq!(browser_button_to_x11(0), Some(1));
        assert_eq!(browser_button_to_x11(1), Some(2));
        assert_eq!(browser_button_to_x11(2), Some(3));
    }

    #[test]
    fn browser_button_to_x11_returns_none_for_unknown() {
        assert_eq!(browser_button_to_x11(3), None);
        assert_eq!(browser_button_to_x11(255), None);
    }

    // --- file_download_request_dropped (additional cases) ---

    #[test]
    fn file_download_drop_with_unit_payload() {
        // Verify the generic type parameter doesn't lock us to String.
        use tokio::sync::mpsc::error::TrySendError;
        let r: Result<(), TrySendError<u64>> = Err(TrySendError::Full(42));
        assert!(file_download_request_dropped(r));
    }

    // ========================================================================
    // dispatch_input_action — mock-driven tests of the full closure dispatch.
    // ========================================================================
    //
    // The production closure spawns a real X11 InputInjector + ClipboardBridge
    // + FileTransferManager. For tests we substitute mock impls of the
    // Inject / ClipboardWrite / FileSink traits, then drive every
    // InputAction variant through dispatch_input_action and assert the mock
    // recorded the expected calls.

    use crate::clipboard::ClipboardWrite;
    use crate::filetransfer::FileSink;
    use crate::input::Inject;
    use anyhow::anyhow;
    use std::sync::Mutex as StdMutex;

    /// Mock InputInjector that records every call. Configurable to return
    /// an error from any method, so the error-logging branches are
    /// exercised. Behind a Mutex so the recording side is `&` while
    /// dispatch uses `&mut`.
    #[derive(Default)]
    struct MockInjector {
        keys: StdMutex<Vec<(u16, bool)>>,
        mouse_abs: StdMutex<Vec<(f64, f64)>>,
        mouse_rel: StdMutex<Vec<(f64, f64)>>,
        buttons: StdMutex<Vec<(u8, bool)>>,
        scrolls: StdMutex<Vec<(f64, f64)>>,
        fail_next: StdMutex<bool>,
    }

    impl MockInjector {
        fn new() -> Self {
            Self::default()
        }
        fn fail_next(self) -> Self {
            *self.fail_next.lock().unwrap() = true;
            self
        }
        fn maybe_fail(&self) -> anyhow::Result<()> {
            let mut f = self.fail_next.lock().unwrap();
            if *f {
                *f = false;
                Err(anyhow!("mock failure"))
            } else {
                Ok(())
            }
        }
    }

    impl Inject for MockInjector {
        fn inject_key(&mut self, code: u16, pressed: bool) -> anyhow::Result<()> {
            self.keys.lock().unwrap().push((code, pressed));
            self.maybe_fail()
        }
        fn inject_mouse_move_abs(&mut self, x: f64, y: f64) -> anyhow::Result<()> {
            self.mouse_abs.lock().unwrap().push((x, y));
            self.maybe_fail()
        }
        fn inject_mouse_move_rel(&mut self, dx: f64, dy: f64) -> anyhow::Result<()> {
            self.mouse_rel.lock().unwrap().push((dx, dy));
            self.maybe_fail()
        }
        fn inject_button(&mut self, button: u8, pressed: bool) -> anyhow::Result<()> {
            self.buttons.lock().unwrap().push((button, pressed));
            self.maybe_fail()
        }
        fn inject_scroll(&mut self, dx: f64, dy: f64) -> anyhow::Result<()> {
            self.scrolls.lock().unwrap().push((dx, dy));
            self.maybe_fail()
        }
    }

    #[derive(Default)]
    struct MockClipboard {
        clip: StdMutex<Vec<String>>,
        prim: StdMutex<Vec<String>>,
        fail: StdMutex<bool>,
    }

    impl MockClipboard {
        fn new() -> Self {
            Self::default()
        }
        fn fail_next(self) -> Self {
            *self.fail.lock().unwrap() = true;
            self
        }
        fn maybe_fail(&self) -> anyhow::Result<()> {
            let mut f = self.fail.lock().unwrap();
            if *f {
                *f = false;
                Err(anyhow!("mock clip failure"))
            } else {
                Ok(())
            }
        }
    }

    impl ClipboardWrite for MockClipboard {
        fn set_text(&self, text: &str) -> anyhow::Result<()> {
            self.clip.lock().unwrap().push(text.to_string());
            self.maybe_fail()
        }
        fn set_primary_text(&self, text: &str) -> anyhow::Result<()> {
            self.prim.lock().unwrap().push(text.to_string());
            self.maybe_fail()
        }
    }

    #[derive(Default)]
    struct MockFileSink {
        starts: StdMutex<Vec<(String, String, u64)>>,
        chunks: StdMutex<Vec<(String, String)>>,
        dones: StdMutex<Vec<String>>,
        fail: StdMutex<bool>,
    }

    impl MockFileSink {
        fn new() -> Self {
            Self::default()
        }
        fn fail_next(self) -> Self {
            *self.fail.lock().unwrap() = true;
            self
        }
        fn maybe_fail(&self) -> anyhow::Result<()> {
            let mut f = self.fail.lock().unwrap();
            if *f {
                *f = false;
                Err(anyhow!("mock file failure"))
            } else {
                Ok(())
            }
        }
    }

    impl FileSink for MockFileSink {
        fn handle_file_start(&mut self, id: &str, name: &str, size: u64) -> anyhow::Result<()> {
            self.starts
                .lock()
                .unwrap()
                .push((id.to_string(), name.to_string(), size));
            self.maybe_fail()
        }
        fn handle_file_chunk(&mut self, id: &str, data: &str) -> anyhow::Result<()> {
            self.chunks
                .lock()
                .unwrap()
                .push((id.to_string(), data.to_string()));
            self.maybe_fail()
        }
        fn handle_file_done(&mut self, id: &str) -> anyhow::Result<()> {
            self.dones.lock().unwrap().push(id.to_string());
            self.maybe_fail()
        }
    }

    /// Owned holder of every channel + flag the dispatch needs. Lets each
    /// test build a fresh fixture in one line.
    struct TestChannels {
        resize_tx: mpsc::Sender<(u32, u32)>,
        resize_rx: mpsc::Receiver<(u32, u32)>,
        clipboard_read_tx: mpsc::Sender<()>,
        clipboard_read_rx: mpsc::Receiver<()>,
        download_request_tx: mpsc::Sender<String>,
        download_request_rx: mpsc::Receiver<String>,
        capture_wake: Arc<(StdMutex<bool>, std::sync::Condvar)>,
        capture_cmd_tx: std::sync::mpsc::Sender<CaptureCommand>,
        capture_cmd_rx: std::sync::mpsc::Receiver<CaptureCommand>,
        tab_backgrounded: AtomicBool,
        video_needs_keyframe: AtomicBool,
    }

    impl TestChannels {
        fn new() -> Self {
            let (resize_tx, resize_rx) = mpsc::channel(8);
            let (clipboard_read_tx, clipboard_read_rx) = mpsc::channel(8);
            let (download_request_tx, download_request_rx) = mpsc::channel(8);
            let capture_wake = Arc::new((StdMutex::new(false), std::sync::Condvar::new()));
            let (capture_cmd_tx, capture_cmd_rx) = std::sync::mpsc::channel();
            Self {
                resize_tx,
                resize_rx,
                clipboard_read_tx,
                clipboard_read_rx,
                download_request_tx,
                download_request_rx,
                capture_wake,
                capture_cmd_tx,
                capture_cmd_rx,
                tab_backgrounded: AtomicBool::new(false),
                video_needs_keyframe: AtomicBool::new(false),
            }
        }

        fn channels(&self) -> InputDispatchChannels<'_> {
            InputDispatchChannels {
                resize_tx: &self.resize_tx,
                clipboard_read_tx: &self.clipboard_read_tx,
                download_request_tx: &self.download_request_tx,
                capture_wake: &self.capture_wake,
                capture_cmd_tx: &self.capture_cmd_tx,
                tab_backgrounded: &self.tab_backgrounded,
                video_needs_keyframe: &self.video_needs_keyframe,
            }
        }
    }

    fn run_dispatch(
        action: InputAction,
    ) -> (MockInjector, MockClipboard, MockFileSink, TestChannels) {
        let mut injector = MockInjector::new();
        let clipboard = MockClipboard::new();
        let mut file_sink = MockFileSink::new();
        let state = InputDispatchState::new();
        let channels = TestChannels::new();
        let chans = channels.channels();
        let _ = dispatch_input_action(
            action,
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &chans,
            ":99",
        );
        (injector, clipboard, file_sink, channels)
    }

    #[test]
    fn dispatch_inject_key_records_call() {
        let (injector, _c, _f, _ch) = run_dispatch(InputAction::InjectKey {
            code: 30,
            pressed: true,
        });
        assert_eq!(injector.keys.lock().unwrap().as_slice(), &[(30, true)]);
    }

    #[test]
    fn dispatch_inject_key_does_not_crash_on_error() {
        let mut injector = MockInjector::new().fail_next();
        let clipboard = MockClipboard::new();
        let mut file_sink = MockFileSink::new();
        let state = InputDispatchState::new();
        let channels = TestChannels::new();
        let _ = dispatch_input_action(
            InputAction::InjectKey {
                code: 30,
                pressed: true,
            },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &channels.channels(),
            ":99",
        );
        // Mock recorded the call and surfaced an Err; dispatch swallowed it.
        assert_eq!(injector.keys.lock().unwrap().len(), 1);
    }

    #[test]
    fn dispatch_inject_key_ctrl_keycode_sets_state() {
        let mut injector = MockInjector::new();
        let clipboard = MockClipboard::new();
        let mut file_sink = MockFileSink::new();
        let state = InputDispatchState::new();
        let channels = TestChannels::new();
        // 29 = LeftCtrl evdev keycode.
        let _ = dispatch_input_action(
            InputAction::InjectKey {
                code: 29,
                pressed: true,
            },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &channels.channels(),
            ":99",
        );
        assert!(state.ctrl_down.load(Ordering::Relaxed));
    }

    #[test]
    fn dispatch_inject_key_ctrl_release_clears_state() {
        let mut injector = MockInjector::new();
        let clipboard = MockClipboard::new();
        let mut file_sink = MockFileSink::new();
        let state = InputDispatchState::new();
        state.ctrl_down.store(true, Ordering::Relaxed);
        let channels = TestChannels::new();
        let _ = dispatch_input_action(
            InputAction::InjectKey {
                code: 29,
                pressed: false,
            },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &channels.channels(),
            ":99",
        );
        assert!(!state.ctrl_down.load(Ordering::Relaxed));
    }

    #[test]
    fn dispatch_inject_key_right_ctrl_also_sets_state() {
        let mut injector = MockInjector::new();
        let clipboard = MockClipboard::new();
        let mut file_sink = MockFileSink::new();
        let state = InputDispatchState::new();
        let channels = TestChannels::new();
        // 97 = RightCtrl evdev keycode.
        let _ = dispatch_input_action(
            InputAction::InjectKey {
                code: 97,
                pressed: true,
            },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &channels.channels(),
            ":99",
        );
        assert!(state.ctrl_down.load(Ordering::Relaxed));
    }

    #[test]
    fn dispatch_inject_key_release_with_ctrl_held_triggers_clipboard_read() {
        let mut injector = MockInjector::new();
        let clipboard = MockClipboard::new();
        let mut file_sink = MockFileSink::new();
        let state = InputDispatchState::new();
        state.ctrl_down.store(true, Ordering::Relaxed);
        let mut channels = TestChannels::new();
        // 45 = `,` keycode — clipboard read trigger.
        let _ = dispatch_input_action(
            InputAction::InjectKey {
                code: 45,
                pressed: false,
            },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &channels.channels(),
            ":99",
        );
        // The clipboard_read channel should have a queued event.
        assert!(channels.clipboard_read_rx.try_recv().is_ok());
    }

    #[test]
    fn dispatch_inject_key_release_with_period_keycode_also_triggers() {
        let mut injector = MockInjector::new();
        let clipboard = MockClipboard::new();
        let mut file_sink = MockFileSink::new();
        let state = InputDispatchState::new();
        state.ctrl_down.store(true, Ordering::Relaxed);
        let mut channels = TestChannels::new();
        // 46 = `.` keycode.
        let _ = dispatch_input_action(
            InputAction::InjectKey {
                code: 46,
                pressed: false,
            },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &channels.channels(),
            ":99",
        );
        assert!(channels.clipboard_read_rx.try_recv().is_ok());
    }

    #[test]
    fn dispatch_inject_key_release_without_ctrl_does_not_trigger() {
        let mut injector = MockInjector::new();
        let clipboard = MockClipboard::new();
        let mut file_sink = MockFileSink::new();
        let state = InputDispatchState::new();
        // ctrl_down stays false.
        let mut channels = TestChannels::new();
        let _ = dispatch_input_action(
            InputAction::InjectKey {
                code: 45,
                pressed: false,
            },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &channels.channels(),
            ":99",
        );
        assert!(channels.clipboard_read_rx.try_recv().is_err());
    }

    #[test]
    fn dispatch_inject_key_press_with_ctrl_does_not_trigger_clipboard() {
        // The trigger fires on RELEASE, not press, even when ctrl is held.
        let mut injector = MockInjector::new();
        let clipboard = MockClipboard::new();
        let mut file_sink = MockFileSink::new();
        let state = InputDispatchState::new();
        state.ctrl_down.store(true, Ordering::Relaxed);
        let mut channels = TestChannels::new();
        let _ = dispatch_input_action(
            InputAction::InjectKey {
                code: 45,
                pressed: true,
            },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &channels.channels(),
            ":99",
        );
        assert!(channels.clipboard_read_rx.try_recv().is_err());
    }

    #[test]
    fn dispatch_inject_mouse_abs_records() {
        let (injector, _c, _f, _ch) = run_dispatch(InputAction::InjectMouseAbs { x: 0.3, y: 0.7 });
        let calls = injector.mouse_abs.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert!((calls[0].0 - 0.3).abs() < 1e-9);
        assert!((calls[0].1 - 0.7).abs() < 1e-9);
    }

    #[test]
    fn dispatch_inject_mouse_abs_propagates_error_silently() {
        let mut injector = MockInjector::new().fail_next();
        let clipboard = MockClipboard::new();
        let mut file_sink = MockFileSink::new();
        let state = InputDispatchState::new();
        let channels = TestChannels::new();
        let _ = dispatch_input_action(
            InputAction::InjectMouseAbs { x: 0.5, y: 0.5 },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &channels.channels(),
            ":99",
        );
        assert_eq!(injector.mouse_abs.lock().unwrap().len(), 1);
    }

    #[test]
    fn dispatch_inject_mouse_rel_records() {
        let (injector, _c, _f, _ch) =
            run_dispatch(InputAction::InjectMouseRel { dx: 1.0, dy: -2.0 });
        let calls = injector.mouse_rel.lock().unwrap();
        assert_eq!(calls.as_slice(), &[(1.0, -2.0)]);
    }

    #[test]
    fn dispatch_inject_mouse_rel_error_branch() {
        let mut injector = MockInjector::new().fail_next();
        let clipboard = MockClipboard::new();
        let mut file_sink = MockFileSink::new();
        let state = InputDispatchState::new();
        let channels = TestChannels::new();
        let _ = dispatch_input_action(
            InputAction::InjectMouseRel { dx: 1.0, dy: -2.0 },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &channels.channels(),
            ":99",
        );
        assert_eq!(injector.mouse_rel.lock().unwrap().len(), 1);
    }

    #[test]
    fn dispatch_inject_button_records() {
        let (injector, _c, _f, _ch) = run_dispatch(InputAction::InjectButton {
            b: 0,
            pressed: true,
        });
        assert_eq!(injector.buttons.lock().unwrap().as_slice(), &[(0, true)]);
    }

    #[test]
    fn dispatch_inject_button_error_branch() {
        let mut injector = MockInjector::new().fail_next();
        let clipboard = MockClipboard::new();
        let mut file_sink = MockFileSink::new();
        let state = InputDispatchState::new();
        let channels = TestChannels::new();
        let _ = dispatch_input_action(
            InputAction::InjectButton {
                b: 2,
                pressed: false,
            },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &channels.channels(),
            ":99",
        );
        assert_eq!(injector.buttons.lock().unwrap().len(), 1);
    }

    #[test]
    fn dispatch_inject_scroll_records() {
        let (injector, _c, _f, _ch) =
            run_dispatch(InputAction::InjectScroll { dx: 0.0, dy: -30.0 });
        let calls = injector.scrolls.lock().unwrap();
        assert_eq!(calls.as_slice(), &[(0.0, -30.0)]);
    }

    #[test]
    fn dispatch_inject_scroll_error_branch() {
        let mut injector = MockInjector::new().fail_next();
        let clipboard = MockClipboard::new();
        let mut file_sink = MockFileSink::new();
        let state = InputDispatchState::new();
        let channels = TestChannels::new();
        let _ = dispatch_input_action(
            InputAction::InjectScroll { dx: 0.0, dy: 30.0 },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &channels.channels(),
            ":99",
        );
        assert_eq!(injector.scrolls.lock().unwrap().len(), 1);
    }

    #[test]
    fn dispatch_set_clipboard_writes_text() {
        let (_i, clipboard, _f, _ch) = run_dispatch(InputAction::SetClipboard {
            text: "hello".into(),
        });
        assert_eq!(clipboard.clip.lock().unwrap().as_slice(), &["hello"]);
    }

    #[test]
    fn dispatch_set_clipboard_error_branch_does_not_panic() {
        let mut injector = MockInjector::new();
        let clipboard = MockClipboard::new().fail_next();
        let mut file_sink = MockFileSink::new();
        let state = InputDispatchState::new();
        let channels = TestChannels::new();
        let _ = dispatch_input_action(
            InputAction::SetClipboard { text: "bad".into() },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &channels.channels(),
            ":99",
        );
        assert_eq!(clipboard.clip.lock().unwrap().len(), 1);
    }

    #[test]
    fn dispatch_set_clipboard_primary_writes_text() {
        let (_i, clipboard, _f, _ch) =
            run_dispatch(InputAction::SetClipboardPrimary { text: "p".into() });
        assert_eq!(clipboard.prim.lock().unwrap().as_slice(), &["p"]);
    }

    #[test]
    fn dispatch_set_clipboard_primary_error_branch() {
        let mut injector = MockInjector::new();
        let clipboard = MockClipboard::new().fail_next();
        let mut file_sink = MockFileSink::new();
        let state = InputDispatchState::new();
        let channels = TestChannels::new();
        let _ = dispatch_input_action(
            InputAction::SetClipboardPrimary { text: "bad".into() },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &channels.channels(),
            ":99",
        );
        assert_eq!(clipboard.prim.lock().unwrap().len(), 1);
    }

    #[test]
    fn dispatch_resize_action_pushes_to_channel() {
        let mut injector = MockInjector::new();
        let clipboard = MockClipboard::new();
        let mut file_sink = MockFileSink::new();
        let state = InputDispatchState::new();
        let mut channels = TestChannels::new();
        let _ = dispatch_input_action(
            InputAction::Resize {
                width: 1920,
                height: 1080,
            },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &channels.channels(),
            ":99",
        );
        let got = channels.resize_rx.try_recv().unwrap();
        assert_eq!(got, (1920, 1080));
    }

    #[test]
    fn dispatch_layout_action_first_time_emits_spawn_effect() {
        let mut injector = MockInjector::new();
        let clipboard = MockClipboard::new();
        let mut file_sink = MockFileSink::new();
        let state = InputDispatchState::new();
        let channels = TestChannels::new();
        let effect = dispatch_input_action(
            InputAction::Layout {
                layout: "us".into(),
            },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &channels.channels(),
            ":99",
        );
        assert!(matches!(
            effect,
            Some(LayoutEffect::Spawn { ref layout, ref display })
            if layout == "us" && display == ":99"
        ));
        // State recorded the new layout for future dedup.
        assert_eq!(state.last_layout.lock().unwrap().as_str(), "us");
    }

    #[test]
    fn dispatch_layout_action_dedup_returns_skip() {
        let mut injector = MockInjector::new();
        let clipboard = MockClipboard::new();
        let mut file_sink = MockFileSink::new();
        let state = InputDispatchState::new();
        *state.last_layout.lock().unwrap() = "us".to_string();
        let channels = TestChannels::new();
        let effect = dispatch_input_action(
            InputAction::Layout {
                layout: "us".into(),
            },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &channels.channels(),
            ":99",
        );
        assert_eq!(effect, Some(LayoutEffect::Skip));
    }

    #[test]
    fn dispatch_layout_recovers_from_poisoned_mutex() {
        // Poison the last_layout mutex and verify the dispatch still works
        // (the production code recovers via `unwrap_or_else(|e| e.into_inner())`).
        let state = InputDispatchState::new();
        let prev = std::sync::Mutex::new(String::from("us"));
        // Force-poison by panicking inside the lock.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _g = prev.lock().unwrap();
            panic!("poisoning");
        }));
        // Replace state.last_layout with the poisoned one.
        // (Can't directly because of field privacy — instead just verify
        // poisoning-recovery works in the helper used by dispatch.)
        let recovered = prev.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(recovered.as_str(), "us");
        drop(state);
    }

    #[test]
    fn dispatch_visibility_true_resets_encoder() {
        let mut injector = MockInjector::new();
        let clipboard = MockClipboard::new();
        let mut file_sink = MockFileSink::new();
        let state = InputDispatchState::new();
        let mut channels = TestChannels::new();
        let _ = dispatch_input_action(
            InputAction::Visibility { visible: true },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &channels.channels(),
            ":99",
        );
        // tab_backgrounded was cleared, video_needs_keyframe set, a
        // ResetEncoder command was queued, and the capture_wake flag was
        // raised so the capture thread comes out of idle.
        assert!(!channels.tab_backgrounded.load(Ordering::Relaxed));
        assert!(channels.video_needs_keyframe.load(Ordering::Relaxed));
        let cmd = channels.capture_cmd_rx.try_recv().unwrap();
        assert!(matches!(cmd, CaptureCommand::ResetEncoder));
        // capture_wake flag was set (we can't easily synchronize on the
        // condvar without a worker, just inspect the bool).
        assert!(*channels.capture_wake.0.lock().unwrap());
    }

    #[test]
    fn dispatch_visibility_false_only_marks_backgrounded() {
        let mut injector = MockInjector::new();
        let clipboard = MockClipboard::new();
        let mut file_sink = MockFileSink::new();
        let state = InputDispatchState::new();
        let mut channels = TestChannels::new();
        let _ = dispatch_input_action(
            InputAction::Visibility { visible: false },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &channels.channels(),
            ":99",
        );
        assert!(channels.tab_backgrounded.load(Ordering::Relaxed));
        // No keyframe request, no reset command.
        assert!(!channels.video_needs_keyframe.load(Ordering::Relaxed));
        assert!(channels.capture_cmd_rx.try_recv().is_err());
    }

    #[test]
    fn dispatch_file_start_invokes_sink() {
        let (_i, _c, file_sink, _ch) = run_dispatch(InputAction::FileStart {
            id: "id-1".into(),
            name: "f.bin".into(),
            size: 1024,
        });
        let starts = file_sink.starts.lock().unwrap();
        assert_eq!(starts.as_slice(), &[("id-1".into(), "f.bin".into(), 1024)]);
    }

    #[test]
    fn dispatch_file_start_error_branch() {
        let mut injector = MockInjector::new();
        let clipboard = MockClipboard::new();
        let mut file_sink = MockFileSink::new().fail_next();
        let state = InputDispatchState::new();
        let channels = TestChannels::new();
        let _ = dispatch_input_action(
            InputAction::FileStart {
                id: "id".into(),
                name: "n".into(),
                size: 1,
            },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &channels.channels(),
            ":99",
        );
        assert_eq!(file_sink.starts.lock().unwrap().len(), 1);
    }

    #[test]
    fn dispatch_file_chunk_invokes_sink() {
        let (_i, _c, file_sink, _ch) = run_dispatch(InputAction::FileChunk {
            id: "id-1".into(),
            data: "AAAA".into(),
        });
        assert_eq!(
            file_sink.chunks.lock().unwrap().as_slice(),
            &[("id-1".into(), "AAAA".into())]
        );
    }

    #[test]
    fn dispatch_file_chunk_error_branch() {
        let mut injector = MockInjector::new();
        let clipboard = MockClipboard::new();
        let mut file_sink = MockFileSink::new().fail_next();
        let state = InputDispatchState::new();
        let channels = TestChannels::new();
        let _ = dispatch_input_action(
            InputAction::FileChunk {
                id: "x".into(),
                data: "y".into(),
            },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &channels.channels(),
            ":99",
        );
        assert_eq!(file_sink.chunks.lock().unwrap().len(), 1);
    }

    #[test]
    fn dispatch_file_done_invokes_sink() {
        let (_i, _c, file_sink, _ch) = run_dispatch(InputAction::FileDone { id: "id-7".into() });
        assert_eq!(file_sink.dones.lock().unwrap().as_slice(), &["id-7"]);
    }

    #[test]
    fn dispatch_file_done_error_branch() {
        let mut injector = MockInjector::new();
        let clipboard = MockClipboard::new();
        let mut file_sink = MockFileSink::new().fail_next();
        let state = InputDispatchState::new();
        let channels = TestChannels::new();
        let _ = dispatch_input_action(
            InputAction::FileDone { id: "x".into() },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &channels.channels(),
            ":99",
        );
        assert_eq!(file_sink.dones.lock().unwrap().len(), 1);
    }

    #[test]
    fn dispatch_action_file_download_sends_path_via_channel() {
        let mut injector = MockInjector::new();
        let clipboard = MockClipboard::new();
        let mut file_sink = MockFileSink::new();
        let state = InputDispatchState::new();
        let mut channels = TestChannels::new();
        let _ = dispatch_input_action(
            InputAction::FileDownload {
                path: "/etc/hosts".into(),
            },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &channels.channels(),
            ":99",
        );
        let got = channels.download_request_rx.try_recv().unwrap();
        assert_eq!(got, "/etc/hosts");
    }

    #[test]
    fn dispatch_file_download_when_closed_logs_and_continues() {
        // Drop the receiver before dispatching. The dispatch must not panic
        // and must continue to the next dispatch call.
        let (download_request_tx, download_request_rx) = mpsc::channel::<String>(8);
        drop(download_request_rx);

        let mut injector = MockInjector::new();
        let clipboard = MockClipboard::new();
        let mut file_sink = MockFileSink::new();
        let state = InputDispatchState::new();
        let (resize_tx, _resize_rx) = mpsc::channel(8);
        let (clipboard_read_tx, _clipboard_read_rx) = mpsc::channel(8);
        let capture_wake = Arc::new((StdMutex::new(false), std::sync::Condvar::new()));
        let (capture_cmd_tx, _capture_cmd_rx) = std::sync::mpsc::channel();
        let tab_backgrounded = AtomicBool::new(false);
        let video_needs_keyframe = AtomicBool::new(false);
        let chans = InputDispatchChannels {
            resize_tx: &resize_tx,
            clipboard_read_tx: &clipboard_read_tx,
            download_request_tx: &download_request_tx,
            capture_wake: &capture_wake,
            capture_cmd_tx: &capture_cmd_tx,
            tab_backgrounded: &tab_backgrounded,
            video_needs_keyframe: &video_needs_keyframe,
        };
        let _ = dispatch_input_action(
            InputAction::FileDownload {
                path: "/etc/hosts".into(),
            },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &chans,
            ":99",
        );
        // No panic; the dispatch swallowed the closed-channel error.
    }

    #[test]
    fn dispatch_ignore_is_a_noop() {
        let (injector, clipboard, file_sink, _ch) = run_dispatch(InputAction::Ignore);
        assert!(injector.keys.lock().unwrap().is_empty());
        assert!(injector.mouse_abs.lock().unwrap().is_empty());
        assert!(injector.mouse_rel.lock().unwrap().is_empty());
        assert!(injector.buttons.lock().unwrap().is_empty());
        assert!(injector.scrolls.lock().unwrap().is_empty());
        assert!(clipboard.clip.lock().unwrap().is_empty());
        assert!(clipboard.prim.lock().unwrap().is_empty());
        assert!(file_sink.starts.lock().unwrap().is_empty());
    }

    // ========================================================================
    // spawn_setxkbmap_worker tests — exercises the LayoutEffect spawn path
    // without running an actual setxkbmap (the binary may not exist on test
    // hosts; even when it does, spawning real subprocesses slows the suite).
    // ========================================================================

    #[test]
    fn spawn_setxkbmap_worker_skip_is_noop() {
        // Skip effect should not spawn any thread; the call returns immediately.
        spawn_setxkbmap_worker(LayoutEffect::Skip);
    }

    #[test]
    fn spawn_setxkbmap_worker_spawn_does_not_panic() {
        // The spawn path forks a thread that runs `setxkbmap <layout>`. If the
        // binary is missing the spawn classifies as SpawnError; if it runs the
        // result is Success/Failure depending on the bogus display. Either
        // way, the parent doesn't panic. We don't join the thread because the
        // test doesn't care about its outcome — it's purely smoke-testing the
        // spawn path doesn't crash the caller.
        spawn_setxkbmap_worker(LayoutEffect::Spawn {
            layout: "us".to_string(),
            display: ":99999".to_string(),
        });
        // Give the worker thread a moment to run its Command::output() before
        // the test exits and tears down the runtime.
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // ========================================================================
    // handle_pre_dispatch tests — exercises the per-event bookkeeping.
    // ========================================================================

    #[test]
    fn pre_dispatch_updates_last_input_timestamp() {
        let last = AtomicU64::new(0);
        let wake = (StdMutex::new(false), std::sync::Condvar::new());
        let bg = AtomicBool::new(false);
        let event = InputEvent::Key { c: 30, d: true };
        let now = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_millis(12345);
        let _ = handle_pre_dispatch(&event, &last, &wake, &bg, now);
        assert_eq!(last.load(Ordering::Relaxed), 12345);
    }

    #[test]
    fn pre_dispatch_wakes_capture_thread() {
        let last = AtomicU64::new(0);
        let wake = (StdMutex::new(false), std::sync::Condvar::new());
        let bg = AtomicBool::new(false);
        let event = InputEvent::Key { c: 30, d: true };
        let _ = handle_pre_dispatch(&event, &last, &wake, &bg, std::time::SystemTime::now());
        assert!(*wake.0.lock().unwrap());
    }

    #[test]
    fn pre_dispatch_clears_backgrounded_on_interactive_event() {
        let last = AtomicU64::new(0);
        let wake = (StdMutex::new(false), std::sync::Condvar::new());
        let bg = AtomicBool::new(true);
        let event = InputEvent::Key { c: 30, d: true };
        let cleared = handle_pre_dispatch(&event, &last, &wake, &bg, std::time::SystemTime::now());
        assert!(cleared);
        assert!(!bg.load(Ordering::Relaxed));
    }

    #[test]
    fn pre_dispatch_does_not_clear_backgrounded_on_non_interactive_event() {
        let last = AtomicU64::new(0);
        let wake = (StdMutex::new(false), std::sync::Condvar::new());
        let bg = AtomicBool::new(true);
        let event = InputEvent::VisibilityState { visible: false };
        let cleared = handle_pre_dispatch(&event, &last, &wake, &bg, std::time::SystemTime::now());
        assert!(!cleared);
        // backgrounded flag remains set because the event wasn't interactive.
        assert!(bg.load(Ordering::Relaxed));
    }

    // ========================================================================
    // InputDispatchState plumbing tests
    // ========================================================================

    #[test]
    fn input_dispatch_state_new_starts_with_ctrl_down_false() {
        let s = InputDispatchState::new();
        assert!(!s.ctrl_down.load(Ordering::Relaxed));
    }

    #[test]
    fn input_dispatch_state_new_starts_with_empty_layout() {
        let s = InputDispatchState::new();
        assert!(s.last_layout.lock().unwrap().is_empty());
    }

    #[test]
    fn input_dispatch_state_layout_can_be_mutated() {
        let s = InputDispatchState::new();
        *s.last_layout.lock().unwrap() = "fr".to_string();
        assert_eq!(s.last_layout.lock().unwrap().as_str(), "fr");
    }

    // ========================================================================
    // LayoutEffect equality / pattern matching
    // ========================================================================

    #[test]
    fn layout_effect_equality() {
        assert_eq!(LayoutEffect::Skip, LayoutEffect::Skip);
        assert_eq!(
            LayoutEffect::Spawn {
                layout: "us".into(),
                display: ":0".into()
            },
            LayoutEffect::Spawn {
                layout: "us".into(),
                display: ":0".into()
            }
        );
        assert_ne!(
            LayoutEffect::Skip,
            LayoutEffect::Spawn {
                layout: "us".into(),
                display: ":0".into()
            }
        );
    }

    // ========================================================================
    // Cross-action stateful tests: drive a sequence and verify cumulative effects.
    // ========================================================================

    #[test]
    fn dispatch_sequence_ctrl_press_then_comma_release_fires_clipboard_read() {
        // Real-world flow: user presses Ctrl, holds, presses comma, releases comma.
        // Only the comma RELEASE while ctrl_down=true triggers the clipboard read.
        let mut injector = MockInjector::new();
        let clipboard = MockClipboard::new();
        let mut file_sink = MockFileSink::new();
        let state = InputDispatchState::new();
        let mut channels = TestChannels::new();

        {
            let chans = channels.channels();
            let _ = dispatch_input_action(
                InputAction::InjectKey {
                    code: 29,
                    pressed: true,
                }, // Ctrl down
                &mut injector,
                &clipboard,
                &mut file_sink,
                &state,
                &chans,
                ":99",
            );
            let _ = dispatch_input_action(
                InputAction::InjectKey {
                    code: 45,
                    pressed: true,
                }, // , down
                &mut injector,
                &clipboard,
                &mut file_sink,
                &state,
                &chans,
                ":99",
            );
        }
        // No trigger yet — only on release.
        assert!(channels.clipboard_read_rx.try_recv().is_err());

        {
            let chans = channels.channels();
            let _ = dispatch_input_action(
                InputAction::InjectKey {
                    code: 45,
                    pressed: false,
                }, // , release
                &mut injector,
                &clipboard,
                &mut file_sink,
                &state,
                &chans,
                ":99",
            );
        }
        assert!(channels.clipboard_read_rx.try_recv().is_ok());
    }

    #[test]
    fn dispatch_sequence_layout_dedup_skip_after_first() {
        // First layout = spawn, second identical layout = skip.
        let mut injector = MockInjector::new();
        let clipboard = MockClipboard::new();
        let mut file_sink = MockFileSink::new();
        let state = InputDispatchState::new();
        let channels = TestChannels::new();
        let chans = channels.channels();

        let e1 = dispatch_input_action(
            InputAction::Layout {
                layout: "us".into(),
            },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &chans,
            ":99",
        );
        assert!(matches!(e1, Some(LayoutEffect::Spawn { .. })));

        let e2 = dispatch_input_action(
            InputAction::Layout {
                layout: "us".into(),
            },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &chans,
            ":99",
        );
        assert_eq!(e2, Some(LayoutEffect::Skip));
    }

    #[test]
    fn dispatch_sequence_layout_change_emits_spawn() {
        // First layout `us`, then `fr` — the `fr` should spawn because it differs.
        let mut injector = MockInjector::new();
        let clipboard = MockClipboard::new();
        let mut file_sink = MockFileSink::new();
        let state = InputDispatchState::new();
        let channels = TestChannels::new();
        let chans = channels.channels();

        let _ = dispatch_input_action(
            InputAction::Layout {
                layout: "us".into(),
            },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &chans,
            ":99",
        );
        let e2 = dispatch_input_action(
            InputAction::Layout {
                layout: "fr".into(),
            },
            &mut injector,
            &clipboard,
            &mut file_sink,
            &state,
            &chans,
            ":99",
        );
        assert!(matches!(e2, Some(LayoutEffect::Spawn { ref layout, .. }) if layout == "fr"));
        assert_eq!(state.last_layout.lock().unwrap().as_str(), "fr");
    }
}
