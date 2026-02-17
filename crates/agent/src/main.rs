mod abr;
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
mod h264;
mod input;
mod peer;
mod signaling;
mod video;

use anyhow::Context;
use audio::AudioCapture;
use beam_protocol::{InputEvent, SignalingMessage};
use capture::ScreenCapture;
use cli::{DEFAULT_FRAMERATE, LOW_BITRATE, LOW_FRAMERATE};
use clipboard::ClipboardBridge;
use encoder::Encoder;
use input::InputInjector;
use peer::{PeerConfig, SharedPeer};
use signaling::SignalingCtx;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

/// Commands sent from async tasks to the capture thread.
/// Using a command channel lets the capture thread exclusively own (and recreate)
/// the Encoder and ScreenCapture during dynamic resolution changes.
pub(crate) enum CaptureCommand {
    SetBitrate(u32),
    Resize {
        width: u32,
        height: u32,
    },
    /// Switch quality mode: true = high (LAN), false = low (WAN)
    SetQualityHigh(bool),
    /// Recreate the encoder pipeline to guarantee a fresh IDR frame.
    /// Used on WebRTC reconnection -- ForceKeyUnit events are unreliable
    /// on nvh264enc after long P-frame runs with gop-size=MAX.
    ResetEncoder,
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
    display: String,
    max_width: u32,
    max_height: u32,
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
        display,
        max_width,
        max_height,
    } = ctx;
    let ctrl_down = Arc::new(AtomicBool::new(false));
    let last_layout = Arc::new(std::sync::Mutex::new(String::new()));

    Arc::new(move |event: InputEvent| {
        // Update last input timestamp for idle detection
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        last_input_time.store(now_ms, Ordering::Relaxed);

        // Wake capture thread if it's sleeping in idle mode
        {
            let (lock, cvar) = &*capture_wake;
            let mut woken = lock.lock().unwrap_or_else(|e| e.into_inner());
            *woken = true;
            cvar.notify_one();
        }

        match event {
            InputEvent::Key { c, d } => {
                if c == 29 || c == 97 {
                    ctrl_down.store(d, Ordering::Relaxed);
                }
                if !d && (c == 46 || c == 45) && ctrl_down.load(Ordering::Relaxed) {
                    let _ = clipboard_read_tx.try_send(());
                }
                if let Err(e) = injector
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .inject_key(c, d)
                {
                    warn!("Key inject error: {e:#}");
                }
            }
            InputEvent::MouseMove { x, y } => {
                if let Err(e) = injector
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .inject_mouse_move_abs(x, y)
                {
                    warn!("Mouse move inject error: {e:#}");
                }
            }
            InputEvent::RelativeMouseMove { dx, dy } => {
                if dx.is_finite()
                    && dy.is_finite()
                    && (-10000.0..=10000.0).contains(&dx)
                    && (-10000.0..=10000.0).contains(&dy)
                    && let Err(e) = injector
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .inject_mouse_move_rel(dx, dy)
                {
                    warn!("Relative mouse move inject error: {e:#}");
                }
            }
            InputEvent::Button { b, d } => {
                if let Err(e) = injector
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .inject_button(b, d)
                {
                    warn!("Button inject error: {e:#}");
                }
            }
            InputEvent::Scroll { dx, dy } => {
                if dx.is_finite()
                    && dy.is_finite()
                    && (-10000.0..=10000.0).contains(&dx)
                    && (-10000.0..=10000.0).contains(&dy)
                    && let Err(e) = injector
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .inject_scroll(dx, dy)
                {
                    warn!("Scroll inject error: {e:#}");
                }
            }
            InputEvent::Clipboard { ref text } => {
                const MAX_CLIPBOARD_BYTES: usize = 1_048_576;
                if text.len() > MAX_CLIPBOARD_BYTES {
                    warn!(
                        len = text.len(),
                        max = MAX_CLIPBOARD_BYTES,
                        "Clipboard text too large, ignoring"
                    );
                } else if let Err(e) = clipboard
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .set_text(text)
                {
                    warn!("Clipboard set error: {e:#}");
                }
            }
            InputEvent::ClipboardPrimary { ref text } => {
                const MAX_CLIPBOARD_BYTES: usize = 1_048_576;
                if text.len() > MAX_CLIPBOARD_BYTES {
                    warn!(
                        len = text.len(),
                        max = MAX_CLIPBOARD_BYTES,
                        "Primary clipboard text too large, ignoring"
                    );
                } else if let Err(e) = clipboard
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .set_primary_text(text)
                {
                    warn!("Primary clipboard set error: {e:#}");
                }
            }
            InputEvent::Resize { w, h } => {
                if let Some((cw, ch)) =
                    display::clamp_resize_dimensions(w, h, max_width, max_height)
                {
                    let _ = resize_tx.try_send((cw, ch));
                } else {
                    warn!(w, h, "Ignoring invalid resize dimensions");
                }
            }
            InputEvent::Layout { ref layout } => {
                if layout.len() <= 20
                    && !layout.is_empty()
                    && layout
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
                {
                    let mut prev = last_layout.lock().unwrap_or_else(|e| e.into_inner());
                    if *prev == *layout {
                        return;
                    }
                    *prev = layout.clone();
                    drop(prev);

                    let display_str = display.clone();
                    let layout = layout.clone();
                    std::thread::spawn(move || {
                        match std::process::Command::new("setxkbmap")
                            .arg(&layout)
                            .env("DISPLAY", &display_str)
                            .output()
                        {
                            Ok(output) if output.status.success() => {
                                info!(layout = %layout, "Keyboard layout set via setxkbmap");
                            }
                            Ok(output) => {
                                let stderr = String::from_utf8_lossy(&output.stderr);
                                warn!(layout = %layout, "setxkbmap failed: {stderr}");
                            }
                            Err(e) => {
                                warn!(layout = %layout, "Failed to run setxkbmap: {e}");
                            }
                        }
                    });
                } else {
                    warn!(layout = %layout, "Invalid keyboard layout name, ignoring");
                }
            }
            InputEvent::Quality { ref mode } => {
                let is_high = mode == "high";
                info!(mode = %mode, "Quality mode changed");
                let _ = capture_cmd_tx.send(CaptureCommand::SetQualityHigh(is_high));
            }
            InputEvent::VisibilityState { visible } => {
                debug!(visible, "Browser tab visibility changed");
                tab_backgrounded.store(!visible, Ordering::Relaxed);
                if visible {
                    // Wake capture thread immediately to restore full framerate
                    let (lock, cvar) = &*capture_wake;
                    let mut woken = lock.lock().unwrap_or_else(|e| e.into_inner());
                    *woken = true;
                    cvar.notify_one();
                }
            }
            InputEvent::FileStart {
                ref id,
                ref name,
                size,
            } => {
                if let Err(e) = file_transfer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .handle_file_start(id, name, size)
                {
                    warn!(id, name, "File transfer start error: {e:#}");
                }
            }
            InputEvent::FileChunk { ref id, ref data } => {
                if let Err(e) = file_transfer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .handle_file_chunk(id, data)
                {
                    warn!(id, "File chunk error: {e:#}");
                }
            }
            InputEvent::FileDone { ref id } => {
                if let Err(e) = file_transfer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .handle_file_done(id)
                {
                    warn!(id, "File done error: {e:#}");
                }
            }
            InputEvent::FileDownloadRequest { ref path } => {
                if let Err(e) = download_request_tx.try_send(path.clone()) {
                    warn!(path, "File download request dropped: {e:#}");
                }
            }
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

    // PulseAudio server path (set when we start a virtual display with PulseAudio)
    let mut pulse_server: Option<String> = None;

    // Try to connect to the display; if it doesn't exist, start a virtual one
    let mut virtual_display = match ScreenCapture::new(&args.display) {
        Ok(_) => {
            info!(display = %args.display, "Connected to existing display");
            None
        }
        Err(e) => {
            warn!(display = %args.display, "Display not available ({e:#}), starting virtual display");
            let display_num: u32 = args.display.trim_start_matches(':').parse().unwrap_or(10);
            match display::VirtualDisplay::start(display_num, args.width, args.height) {
                Ok(mut vd) => {
                    info!(display = %args.display, "Virtual display started");

                    // Start PulseAudio BEFORE desktop so apps inherit PULSE_SERVER.
                    // Without this, desktop apps connect to the system PulseAudio
                    // instead of the beam null-sink, and audio isn't captured.
                    if let Err(e) = vd.start_pulseaudio() {
                        warn!("Failed to start PulseAudio: {e:#}");
                    }
                    let pulse_path = format!("/tmp/beam-pulse-{display_num}/native");
                    pulse_server = Some(format!("unix:{pulse_path}"));
                    // Wait for PulseAudio socket to appear (up to 2s)
                    for _ in 0..20 {
                        if std::path::Path::new(&pulse_path).exists() {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }

                    // Start desktop AFTER PulseAudio (PULSE_SERVER passed explicitly)
                    if let Err(e) = vd.start_desktop() {
                        warn!("Failed to start desktop: {e:#}");
                    }
                    // Note: cursor is hidden by the cursor monitor thread
                    // (XFixes HideCursor) which also tracks cursor shape changes.
                    // Brief wait for desktop to initialize (Xorg is already running,
                    // just need window manager to start drawing)
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                    Some(vd)
                }
                Err(e) => {
                    return Err(e).context("Failed to start virtual display");
                }
            }
        }
    };

    // Create screen capture (now the display should be available)
    let mut screen_capture =
        ScreenCapture::new(&args.display).context("Failed to initialize screen capture")?;
    let width = screen_capture.width();
    let height = screen_capture.height();

    // Create encoder (use config values from server)
    let initial_framerate = args.framerate;
    let initial_bitrate = args.bitrate;
    let encoder = Encoder::with_encoder_preference(
        width,
        height,
        initial_framerate,
        initial_bitrate,
        args.encoder.as_deref(),
    )
    .context("Failed to initialize encoder")?;
    let encoder_type = encoder.encoder_type();
    let encoder_pref = args.encoder.clone();

    // Parse ICE server config
    let ice_servers = match &args.ice_servers_json {
        Some(json) => {
            #[derive(serde::Deserialize)]
            struct IceServerEntry {
                urls: Vec<String>,
                username: Option<String>,
                credential: Option<String>,
            }
            let entries: Vec<IceServerEntry> =
                serde_json::from_str(json).context("Invalid --ice-servers JSON")?;
            entries
                .into_iter()
                .map(|e| peer::IceServerConfig {
                    urls: e.urls,
                    username: e.username,
                    credential: e.credential,
                })
                .collect()
        }
        None => Vec::new(),
    };

    // Bundle peer creation parameters for reuse on browser reconnect
    let peer_config = PeerConfig {
        ice_servers,
        encoder_type,
    };

    // Config-driven values for capture/encode
    let config_bitrate = args.bitrate;
    let config_framerate = args.framerate;
    let config_min_bitrate = args.min_bitrate;
    let config_max_bitrate = args.max_bitrate;

    // Channel for encoded video frames: capture thread -> async write loop
    // Capacity of 2: reduces frame drops under burst while keeping latency low.
    // Combined with max-buffers=1 on appsink, total pipeline depth is at most
    // 3 frames (~25ms at 120fps). Frames are dropped rather than queued when
    // the channel is full.
    let (encoded_tx, mut encoded_rx) = mpsc::channel::<Vec<u8>>(2);

    // Channel for encoded audio frames: audio thread -> async write loop
    let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(8);

    // Channel for signaling messages to send back to server
    let (signal_tx, mut signal_rx) = mpsc::channel::<SignalingMessage>(32);

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

    // The capture thread exclusively owns the Encoder so it can recreate it
    // during dynamic resolution changes. Other tasks communicate via channels.

    // Force-keyframe flag: set from RTCP reader and signaling handler,
    // cleared by capture thread each frame. AtomicBool avoids the Sync
    // requirement that would prevent std::sync::mpsc::Sender in callbacks.
    let force_keyframe = Arc::new(AtomicBool::new(false));

    // Command channel for non-latency-critical capture thread operations
    let (capture_cmd_tx, capture_cmd_rx) = std::sync::mpsc::channel::<CaptureCommand>();

    // Resize request channel
    let (resize_tx, mut resize_rx) = mpsc::channel::<(u32, u32)>(4);

    // Idle detection: track last input time for framerate reduction
    // Uses epoch millis stored as AtomicU64 for lock-free access from capture thread
    let last_input_time = Arc::new(AtomicU64::new(0));
    let last_input_for_capture = Arc::clone(&last_input_time);

    // Wake signal: input callback signals capture thread to wake immediately
    // when input arrives during idle mode (avoids up to 100ms delay at 10fps)
    let capture_wake = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let capture_wake_for_input = Arc::clone(&capture_wake);

    // Channel for clipboard read requests (Ctrl+C/X detection -> read X11 clipboard)
    let (clipboard_read_tx, mut clipboard_read_rx) = mpsc::channel::<()>(4);

    // Channel for file download requests (browser -> agent -> browser via DataChannel)
    let (download_request_tx, mut download_request_rx) = mpsc::channel::<String>(4);

    // Cursor shape monitor: tracks X11 cursor changes via XFixes and sends
    // CSS cursor names. Also hides the X11 cursor via XFixes HideCursor.
    let mut cursor_rx = cursor::spawn_cursor_monitor(&args.display);
    if cursor_rx.is_none() {
        warn!("Cursor monitor failed to start, falling back to unclutter");
        if let Some(ref mut vd) = virtual_display {
            vd.hide_cursor();
        }
    }

    // Tab backgrounded flag: set by visibility state events from browser,
    // checked by capture thread to reduce framerate when tab is hidden.
    let tab_backgrounded = Arc::new(AtomicBool::new(false));
    let tab_backgrounded_for_capture = Arc::clone(&tab_backgrounded);

    // File transfer manager -- writes uploads to ~/Downloads
    let home_dir = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/tmp"));
    let file_transfer = Arc::new(Mutex::new(filetransfer::FileTransferManager::new(home_dir)));

    // Build the reusable input callback (Arc<dyn Fn>) so it survives peer recreation
    // Keep a reference to file_transfer for the download handler
    let file_transfer_for_download = Arc::clone(&file_transfer);

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
        display: args.display.clone(),
        max_width: args.max_width,
        max_height: args.max_height,
    });

    // Create initial WebRTC peer with all callbacks
    let initial_peer = peer::create_peer(
        &peer_config,
        &signal_tx,
        session_id,
        &force_keyframe,
        Arc::clone(&input_callback),
    )
    .await?;
    let shared_peer: SharedPeer = Arc::new(tokio::sync::RwLock::new(initial_peer));

    // Shutdown flag for capture/audio threads
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_for_capture = Arc::clone(&shutdown);
    let shutdown_for_audio = Arc::clone(&shutdown);

    // Capture + encode thread
    const IDLE_TIMEOUT_MS: u64 = 300_000; // 5 minutes of no input -> idle mode
    const IDLE_FRAMERATE: u32 = 5; // Low framerate when no input -- saves GPU/CPU
    const BACKGROUND_FRAMERATE: u32 = 1; // Minimal framerate when browser tab is hidden
    const ENCODER_RESET_COOLDOWN: Duration = Duration::from_secs(5); // Throttle encoder recreation to protect NVENC

    let display_for_capture = args.display.clone();
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
                let param = libc::sched_param { sched_priority: 50 };
                let ret = unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) };
                if ret != 0 {
                    warn!("Could not set SCHED_FIFO (need CAP_SYS_NICE): {}",
                        std::io::Error::last_os_error());
                } else {
                    info!("Capture thread elevated to SCHED_FIFO priority 50");
                }
            }

            // The capture thread exclusively owns screen_capture and encoder
            // so it can recreate them during dynamic resolution changes.
            let mut encoder = encoder;
            let mut current_bitrate = config_bitrate;
            let mut current_framerate = config_framerate;
            let mut active_frame_duration_ns = 1_000_000_000u64 / config_framerate as u64;
            let idle_frame_duration_ns = 1_000_000_000u64 / IDLE_FRAMERATE as u64;
            let background_frame_duration_ns = 1_000_000_000u64 / BACKGROUND_FRAMERATE as u64;
            let mut frame_count: u64 = 0;
            let mut encoded_count: u64 = 0;
            let start = Instant::now();
            let mut was_idle = false;
            let mut was_backgrounded = false;
            let mut first_capture_logged = false;
            let mut first_encode_logged = false;
            let mut last_encoder_reset = Instant::now() - ENCODER_RESET_COOLDOWN; // Allow first reset immediately

            loop {
                if shutdown_for_capture.load(Ordering::Relaxed) {
                    info!("Capture thread shutting down");
                    break;
                }

                // Process commands from async tasks (non-blocking)
                let abr_enabled = !matches!(encoder_type, encoder::EncoderType::Nvidia);
                while let Ok(cmd) = capture_cmd_rx.try_recv() {
                    match cmd {
                        CaptureCommand::SetBitrate(br) => {
                            if abr_enabled {
                                encoder.set_bitrate(br);
                                current_bitrate = br;
                            }
                        }
                        CaptureCommand::Resize { width, height } => {
                            // Skip no-op resizes (e.g. spurious ResizeObserver events)
                            if width == screen_capture.width() && height == screen_capture.height() {
                                debug!(width, height, "Resize skipped (same dimensions)");
                                continue;
                            }
                            info!(width, height, "Processing resize request");

                            // 1. Change X display resolution via xrandr
                            if let Err(e) = display::set_display_resolution(
                                &display_for_capture, width, height,
                            ) {
                                warn!("xrandr resize failed: {e:#}");
                                continue;
                            }

                            // 2. Wait for X server to apply the mode change.
                            // Check shutdown flag to avoid blocking during teardown.
                            for _ in 0..20 {
                                std::thread::sleep(Duration::from_millis(10));
                                if shutdown_for_capture.load(Ordering::Relaxed) {
                                    return;
                                }
                            }

                            // 3. Recreate screen capture at new resolution
                            let new_capture = match ScreenCapture::new(&display_for_capture) {
                                Ok(cap) => cap,
                                Err(e) => {
                                    error!("Failed to recreate capture after resize: {e:#}");
                                    return;
                                }
                            };
                            screen_capture = new_capture;

                            // 4. Recreate encoder pipeline at new dimensions
                            let new_w = screen_capture.width();
                            let new_h = screen_capture.height();
                            let new_encoder = match Encoder::with_encoder_preference(
                                new_w, new_h, DEFAULT_FRAMERATE, current_bitrate,
                                encoder_pref.as_deref(),
                            ) {
                                Ok(enc) => enc,
                                Err(e) => {
                                    error!("Failed to recreate encoder after resize: {e:#}");
                                    return;
                                }
                            };
                            encoder = new_encoder;

                            // 5. Force IDR so browser decoder reinitializes with new SPS/PPS
                            encoder.force_keyframe();
                            first_capture_logged = false;
                            first_encode_logged = false;

                            // 6. Update input injector dimensions for mouse coordinate mapping
                            input_width_for_capture.store(new_w, Ordering::Relaxed);
                            input_height_for_capture.store(new_h, Ordering::Relaxed);

                            info!(
                                width = new_w, height = new_h,
                                "Resize complete, capture and encoder recreated"
                            );
                        }
                        CaptureCommand::SetQualityHigh(is_high) => {
                            let new_framerate = if is_high { config_framerate } else { LOW_FRAMERATE };
                            if abr_enabled {
                                let new_bitrate = if is_high { config_bitrate } else { LOW_BITRATE };
                                encoder.set_bitrate(new_bitrate);
                                current_bitrate = new_bitrate;
                                info!(bitrate = new_bitrate, fps = new_framerate, high = is_high, "Quality mode applied");
                            } else {
                                info!(fps = new_framerate, high = is_high, "Quality mode applied (framerate only, NVIDIA bitrate locked)");
                            }
                            current_framerate = new_framerate;
                            active_frame_duration_ns = 1_000_000_000u64 / new_framerate as u64;
                            encoder.force_keyframe();
                        }
                        CaptureCommand::ResetEncoder => {
                            let elapsed = last_encoder_reset.elapsed();
                            if elapsed < ENCODER_RESET_COOLDOWN {
                                debug!(
                                    cooldown_remaining_ms = (ENCODER_RESET_COOLDOWN - elapsed).as_millis() as u64,
                                    "ResetEncoder throttled, sending force_keyframe instead"
                                );
                                encoder.force_keyframe();
                            } else {
                                info!("Recreating encoder pipeline for fresh IDR (reconnection)");
                                let new_encoder = match Encoder::with_encoder_preference(
                                    screen_capture.width(),
                                    screen_capture.height(),
                                    current_framerate,
                                    current_bitrate,
                                    encoder_pref.as_deref(),
                                ) {
                                    Ok(enc) => enc,
                                    Err(e) => {
                                        error!("Failed to recreate encoder: {e:#}");
                                        continue;
                                    }
                                };
                                encoder = new_encoder;
                                first_encode_logged = false;
                                last_encoder_reset = Instant::now();
                                info!("Encoder pipeline recreated (next frame will be IDR)");
                            }
                        }
                    }
                }

                // Check force-keyframe flag (set by RTCP reader or signaling)
                if kf_flag_for_capture.swap(false, Ordering::Relaxed) {
                    encoder.force_keyframe();
                }

                let frame_start = Instant::now();
                let pts = start.elapsed().as_nanos() as u64;

                // Check background state: browser tab hidden -> minimal framerate
                let is_backgrounded = tab_backgrounded_for_capture.load(Ordering::Relaxed);

                // Check idle state: if no input for IDLE_TIMEOUT_MS, reduce framerate
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let last_input_ms = last_input_for_capture.load(Ordering::Relaxed);
                let is_idle = last_input_ms > 0 && (now_ms - last_input_ms) > IDLE_TIMEOUT_MS;

                // Background mode takes priority over idle mode (even lower framerate)
                let frame_duration_ns = if is_backgrounded {
                    background_frame_duration_ns
                } else if is_idle {
                    idle_frame_duration_ns
                } else {
                    active_frame_duration_ns
                };

                if is_backgrounded != was_backgrounded {
                    if is_backgrounded {
                        debug!("Tab backgrounded, reducing to {BACKGROUND_FRAMERATE}fps");
                    } else {
                        debug!(fps = current_framerate, "Tab foregrounded, restoring framerate");
                    }
                    was_backgrounded = is_backgrounded;
                }

                if is_idle != was_idle && !is_backgrounded {
                    if is_idle {
                        debug!("Entering idle mode ({IDLE_FRAMERATE}fps)");
                    } else {
                        debug!(fps = current_framerate, "Resuming active mode");
                    }
                    was_idle = is_idle;
                }

                // Auto-recover from GStreamer pipeline errors (GPU fault, driver issue, etc.)
                if encoder.has_error() {
                    warn!("GStreamer pipeline error detected, recreating encoder");
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
                        if !first_capture_logged {
                            info!(size = frame.len(), "First frame captured from X display");
                            first_capture_logged = true;
                        }
                        if let Err(e) = encoder.encode_frame(frame, pts) {
                            error!("Encode error: {e:#}");
                            break;
                        }
                    }
                    Err(e) => {
                        // SHM GetImage can fail with Match error when xrandr changes
                        // the display resolution. Log and skip the frame instead of
                        // crashing - the next frame will likely succeed or the pipeline
                        // will be restarted.
                        debug!("Capture frame skipped: {e:#}");
                        continue;
                    }
                }

                // Drain encoded frames. NVENC is async -- the frame we just
                // pushed may not be ready yet. Spin-poll for up to 2ms to catch
                // it in this iteration rather than waiting for the next capture
                // cycle (which adds up to 8.3ms of unnecessary latency at 120fps).
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
                            // Brief yield while waiting for encoder
                            std::hint::spin_loop();
                        }
                        Err(e) => {
                            error!("Pull encoded error: {e:#}");
                            return;
                        }
                    }
                }

                frame_count += 1;
                if frame_count.is_multiple_of(300) {
                    let elapsed = start.elapsed().as_secs_f64();
                    debug!(
                        captured = frame_count,
                        encoded = encoded_count,
                        fps = format!("{:.1}", frame_count as f64 / elapsed),
                        "Capture stats"
                    );
                }

                // Frame pacing: use condvar wait so input can wake us immediately.
                // During active mode (60fps), use sleep+spin-wait for precise timing.
                // During idle/background mode, use condvar wait that can be interrupted.
                let target = Duration::from_nanos(frame_duration_ns);
                let elapsed = frame_start.elapsed();
                if elapsed < target {
                    let remaining = target - elapsed;
                    if is_idle || is_backgrounded {
                        // Idle/background mode: interruptible wait -- visibility change
                        // or input wakes us immediately
                        let (lock, cvar) = &*capture_wake_for_thread;
                        let mut woken = lock.lock().unwrap_or_else(|e| e.into_inner());
                        *woken = false; // clear any stale wake signal
                        let result = cvar.wait_timeout(woken, remaining)
                            .unwrap_or_else(|e| e.into_inner());
                        if *result.0 {
                            debug!("Capture thread woken by input/visibility change");
                        }
                    } else {
                        // Active mode: precise sleep+spin-wait for frame timing
                        if remaining > Duration::from_millis(2) {
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

    // Audio capture thread
    let audio_handle = match AudioCapture::new(48000, 2, pulse_server.as_deref()) {
        Ok(mut audio_capture) => {
            let handle = std::thread::Builder::new()
                .name("audio-capture".into())
                .spawn(move || {
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
            Some(handle)
        }
        Err(e) => {
            warn!("Audio capture unavailable: {e:#}. Continuing without audio.");
            None
        }
    };

    // Connect to signaling server
    let server_url = args.server_url.clone();
    let kf_flag_for_signal = Arc::clone(&force_keyframe);

    // Adaptive bitrate controller
    let cmd_tx_for_abr = capture_cmd_tx.clone();

    // Signaling handler needs to reset encoder on reconnection
    let cmd_tx_for_signal = capture_cmd_tx.clone();

    // Video send loop needs to reset encoder when IDR wait exhausts
    let cmd_tx_for_video = capture_cmd_tx.clone();

    // Resize forwarder (from tokio channel to capture thread)
    let cmd_tx_for_resize = capture_cmd_tx;

    // Clipboard sync (remote -> browser)
    let clipboard_for_sync = Arc::clone(&clipboard);

    // Set up SIGTERM handler
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;

    let signaling_ctx = SignalingCtx {
        server_url: &server_url,
        session_id,
        agent_token: args.agent_token.as_deref(),
        tls_cert_path: args.tls_cert_path.as_deref(),
        shared_peer: &shared_peer,
        peer_config: &peer_config,
        signal_tx: &signal_tx,
        force_keyframe: kf_flag_for_signal,
        input_callback: Arc::clone(&input_callback),
        capture_cmd_tx: &cmd_tx_for_signal,
    };

    tokio::select! {
        // Write encoded video frames to WebRTC (only when connected)
        _ = video::run_video_send_loop(
            &mut encoded_rx,
            &shared_peer,
            &force_keyframe,
            &cmd_tx_for_video,
            config_framerate,
        ) => {}

        // Write encoded audio frames to WebRTC
        _ = video::run_audio_send_loop(
            &mut audio_rx,
            &shared_peer,
        ) => {}

        // Handle signaling WebSocket
        _ = signaling::run_signaling(
            &signaling_ctx,
            &mut signal_rx,
        ) => {}

        // Forward resize requests from input handler to capture thread
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
            &shared_peer,
        ) => {}

        // File download: read file on blocking thread, stream chunks via DataChannel
        _ = file_transfer_task::run_file_download_loop(
            &mut download_request_rx,
            &file_transfer_for_download,
            &shared_peer,
        ) => {}

        // Cursor shape passthrough: forward X11 cursor changes to browser as CSS cursor values
        _ = async {
            if let Some(ref mut rx) = cursor_rx {
                while let Some(css) = rx.recv().await {
                    let msg = serde_json::json!({ "t": "cur", "css": css }).to_string();
                    let current_peer = peer::snapshot(&shared_peer).await;
                    if let Err(e) = current_peer.send_data_channel_message(&msg).await {
                        debug!("Failed to send cursor shape to browser: {e:#}");
                    }
                }
            } else {
                // No cursor monitor, sleep forever
                std::future::pending::<()>().await;
            }
        } => {}

        // Adaptive bitrate control loop
        _ = abr::run_abr_loop(
            &shared_peer,
            encoder_type,
            config_bitrate,
            config_min_bitrate,
            config_max_bitrate,
            &cmd_tx_for_abr,
        ) => {}

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
    // Drop receivers to unblock any blocking_send in capture threads
    drop(encoded_rx);
    drop(audio_rx);
    // Wait for threads to finish (prevents crash from accessing dead X display)
    if let Err(e) = capture_handle.join() {
        warn!("Capture thread panicked: {e:?}");
    }
    if let Some(handle) = audio_handle
        && let Err(e) = handle.join()
    {
        warn!("Audio thread panicked: {e:?}");
    }

    peer::snapshot(&shared_peer).await.close().await?;
    info!("Agent shutdown complete");
    Ok(())
}
