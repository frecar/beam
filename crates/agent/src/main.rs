mod audio;
mod capture;
mod clipboard;
mod cursor;
mod display;
mod encoder;
mod h264;
mod input;
mod peer;

use anyhow::Context;
use audio::AudioCapture;
use beam_protocol::{AgentCommand, InputEvent, SignalingMessage};
use capture::ScreenCapture;
use clipboard::ClipboardBridge;
use encoder::Encoder;
use input::InputInjector;
use peer::{PeerConfig, SharedPeer};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

const DEFAULT_BITRATE: u32 = 100_000; // 100 Mbps — for VA-API/software encoders
const LOW_BITRATE: u32 = 5_000; // 5 Mbps — for constrained WAN connections
const DEFAULT_FRAMERATE: u32 = 60; // 60fps — higher rates cause WebRTC bufferbloat (RTT spikes to seconds)
const LOW_FRAMERATE: u32 = 30; // 30fps for low quality mode
/// Shared context for building the input event callback.
struct InputCallbackCtx {
    injector: Arc<Mutex<InputInjector>>,
    clipboard: Arc<Mutex<ClipboardBridge>>,
    resize_tx: mpsc::Sender<(u32, u32)>,
    last_input_time: Arc<AtomicU64>,
    clipboard_read_tx: mpsc::Sender<()>,
    capture_wake: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    capture_cmd_tx: std::sync::mpsc::Sender<CaptureCommand>,
    display: String,
}

/// Build the reusable input event callback that dispatches input events
/// to the appropriate subsystem (uinput, clipboard, resize, layout, quality).
fn build_input_callback(ctx: InputCallbackCtx) -> Arc<dyn Fn(InputEvent) + Send + Sync> {
    let InputCallbackCtx {
        injector,
        clipboard,
        resize_tx,
        last_input_time,
        clipboard_read_tx,
        capture_wake,
        capture_cmd_tx,
        display,
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
            InputEvent::Resize { w, h } => {
                if (320..=7680).contains(&w) && (240..=4320).contains(&h) {
                    let _ = resize_tx.try_send((w, h));
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
        }
    })
}

/// Commands sent from async tasks to the capture thread.
/// Using a command channel lets the capture thread exclusively own (and recreate)
/// the Encoder and ScreenCapture during dynamic resolution changes.
enum CaptureCommand {
    SetBitrate(u32),
    Resize {
        width: u32,
        height: u32,
    },
    /// Switch quality mode: true = high (LAN), false = low (WAN)
    SetQualityHigh(bool),
    /// Recreate the encoder pipeline to guarantee a fresh IDR frame.
    /// Used on WebRTC reconnection — ForceKeyUnit events are unreliable
    /// on nvh264enc after long P-frame runs with gop-size=MAX.
    ResetEncoder,
}

struct Args {
    display: String,
    server_url: String,
    session_id: Uuid,
    ice_servers_json: Option<String>,
    agent_token: Option<String>,
    tls_cert_path: Option<String>,
    width: u32,
    height: u32,
    framerate: u32,
    bitrate: u32,
    min_bitrate: u32,
    max_bitrate: u32,
    encoder: Option<String>,
}

fn parse_args() -> anyhow::Result<Args> {
    let mut display = ":0".to_string();
    let mut server_url = String::new();
    let mut session_id = None;
    let mut ice_servers_json = None;
    let mut agent_token = None;
    let mut tls_cert_path = None;
    let mut width: u32 = 1920;
    let mut height: u32 = 1080;
    let mut framerate: u32 = DEFAULT_FRAMERATE;
    let mut bitrate: u32 = DEFAULT_BITRATE;
    let mut min_bitrate: u32 = 5_000;
    let mut max_bitrate: u32 = 80_000;
    let mut encoder: Option<String> = None;

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--display" => {
                i += 1;
                display = args.get(i).context("Missing --display value")?.clone();
            }
            "--server-url" => {
                i += 1;
                server_url = args.get(i).context("Missing --server-url value")?.clone();
            }
            "--session-id" => {
                i += 1;
                session_id = Some(
                    args.get(i)
                        .context("Missing --session-id value")?
                        .parse::<Uuid>()
                        .context("Invalid session-id UUID")?,
                );
            }
            "--ice-servers" => {
                i += 1;
                ice_servers_json =
                    Some(args.get(i).context("Missing --ice-servers value")?.clone());
            }
            "--agent-token" => {
                // Legacy CLI support (prefer BEAM_AGENT_TOKEN env var)
                i += 1;
                agent_token = Some(args.get(i).context("Missing --agent-token value")?.clone());
            }
            "--tls-cert" => {
                i += 1;
                tls_cert_path = Some(args.get(i).context("Missing --tls-cert value")?.clone());
            }
            "--width" => {
                i += 1;
                width = args
                    .get(i)
                    .context("Missing --width value")?
                    .parse()
                    .context("Invalid --width value")?;
            }
            "--height" => {
                i += 1;
                height = args
                    .get(i)
                    .context("Missing --height value")?
                    .parse()
                    .context("Invalid --height value")?;
            }
            "--framerate" => {
                i += 1;
                framerate = args
                    .get(i)
                    .context("Missing --framerate value")?
                    .parse()
                    .context("Invalid --framerate value")?;
            }
            "--bitrate" => {
                i += 1;
                bitrate = args
                    .get(i)
                    .context("Missing --bitrate value")?
                    .parse()
                    .context("Invalid --bitrate value")?;
            }
            "--min-bitrate" => {
                i += 1;
                min_bitrate = args
                    .get(i)
                    .context("Missing --min-bitrate value")?
                    .parse()
                    .context("Invalid --min-bitrate value")?;
            }
            "--max-bitrate" => {
                i += 1;
                max_bitrate = args
                    .get(i)
                    .context("Missing --max-bitrate value")?
                    .parse()
                    .context("Invalid --max-bitrate value")?;
            }
            "--encoder" => {
                i += 1;
                encoder = Some(args.get(i).context("Missing --encoder value")?.clone());
            }
            other => anyhow::bail!("Unknown argument: {other}"),
        }
        i += 1;
    }

    // Prefer env var for agent token (CLI args are visible in /proc)
    if agent_token.is_none() {
        agent_token = std::env::var("BEAM_AGENT_TOKEN").ok();
    }

    Ok(Args {
        display,
        server_url,
        session_id: session_id.context("--session-id is required")?,
        ice_servers_json,
        agent_token,
        tls_cert_path,
        width,
        height,
        framerate,
        bitrate,
        min_bitrate,
        max_bitrate,
        encoder,
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

    let args = parse_args()?;
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

    // Channel for encoded video frames: capture thread → async write loop
    // Capacity of 2: reduces frame drops under burst while keeping latency low.
    // Combined with max-buffers=1 on appsink, total pipeline depth is at most
    // 3 frames (~25ms at 120fps). Frames are dropped rather than queued when
    // the channel is full.
    let (encoded_tx, mut encoded_rx) = mpsc::channel::<Vec<u8>>(2);

    // Channel for encoded audio frames: audio thread → async write loop
    let (audio_tx, mut audio_rx) = mpsc::channel::<Vec<u8>>(8);

    // Channel for signaling messages to send back to server
    let (signal_tx, mut signal_rx) = mpsc::channel::<SignalingMessage>(32);

    let session_id = args.session_id;

    // Create input injector
    let injector = Arc::new(Mutex::new(
        InputInjector::new().context("Failed to create input injector")?,
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

    // Channel for clipboard read requests (Ctrl+C/X detection → read X11 clipboard)
    let (clipboard_read_tx, mut clipboard_read_rx) = mpsc::channel::<()>(4);

    // Cursor shape monitor: tracks X11 cursor changes via XFixes and sends
    // CSS cursor names. Also hides the X11 cursor via XFixes HideCursor.
    let mut cursor_rx = cursor::spawn_cursor_monitor(&args.display);
    if cursor_rx.is_none() {
        warn!("Cursor monitor failed to start, falling back to unclutter");
        if let Some(ref mut vd) = virtual_display {
            vd.hide_cursor();
        }
    }

    // Build the reusable input callback (Arc<dyn Fn>) so it survives peer recreation
    let input_callback = build_input_callback(InputCallbackCtx {
        injector: Arc::clone(&injector),
        clipboard: Arc::clone(&clipboard),
        resize_tx: resize_tx.clone(),
        last_input_time: Arc::clone(&last_input_time),
        clipboard_read_tx: clipboard_read_tx.clone(),
        capture_wake: Arc::clone(&capture_wake_for_input),
        capture_cmd_tx: capture_cmd_tx.clone(),
        display: args.display.clone(),
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
    const IDLE_TIMEOUT_MS: u64 = 300_000; // 5 minutes of no input → idle mode
    const IDLE_FRAMERATE: u32 = 5; // Low framerate when no input — saves GPU/CPU

    let display_for_capture = args.display.clone();
    let kf_flag_for_capture = Arc::clone(&force_keyframe);
    let capture_wake_for_thread = Arc::clone(&capture_wake);

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
            let mut frame_count: u64 = 0;
            let mut encoded_count: u64 = 0;
            let start = Instant::now();
            let mut was_idle = false;
            let mut first_capture_logged = false;
            let mut first_encode_logged = false;

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
                            let new_encoder = match Encoder::new(
                                new_w, new_h, DEFAULT_FRAMERATE, current_bitrate,
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
                            info!("Recreating encoder pipeline for fresh IDR (reconnection)");
                            let new_encoder = match Encoder::new(
                                screen_capture.width(),
                                screen_capture.height(),
                                current_framerate,
                                current_bitrate,
                            ) {
                                Ok(enc) => enc,
                                Err(e) => {
                                    error!("Failed to recreate encoder: {e:#}");
                                    continue;
                                }
                            };
                            encoder = new_encoder;
                            first_encode_logged = false;
                            info!("Encoder pipeline recreated (next frame will be IDR)");
                        }
                    }
                }

                // Check force-keyframe flag (set by RTCP reader or signaling)
                if kf_flag_for_capture.swap(false, Ordering::Relaxed) {
                    encoder.force_keyframe();
                }

                let frame_start = Instant::now();
                let pts = start.elapsed().as_nanos() as u64;

                // Check idle state: if no input for IDLE_TIMEOUT_MS, reduce framerate
                let now_ms = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let last_input_ms = last_input_for_capture.load(Ordering::Relaxed);
                let is_idle = last_input_ms > 0 && (now_ms - last_input_ms) > IDLE_TIMEOUT_MS;
                let frame_duration_ns = if is_idle {
                    idle_frame_duration_ns
                } else {
                    active_frame_duration_ns
                };

                if is_idle != was_idle {
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
                    match Encoder::new(
                        screen_capture.width(), screen_capture.height(),
                        current_framerate, current_bitrate,
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

                // Drain encoded frames. NVENC is async — the frame we just
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
                // During idle mode, use condvar wait that input can interrupt instantly.
                let target = Duration::from_nanos(frame_duration_ns);
                let elapsed = frame_start.elapsed();
                if elapsed < target {
                    let remaining = target - elapsed;
                    if is_idle {
                        // Idle mode: interruptible wait — input wakes us immediately
                        let (lock, cvar) = &*capture_wake_for_thread;
                        let mut woken = lock.lock().unwrap_or_else(|e| e.into_inner());
                        *woken = false; // clear any stale wake signal
                        let result = cvar.wait_timeout(woken, remaining)
                            .unwrap_or_else(|e| e.into_inner());
                        if *result.0 {
                            debug!("Capture thread woken by input (idle → active)");
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

    // Clipboard sync (remote → browser)
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
        _ = async {
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
                let (current_peer, peer_gen) = peer::snapshot_with_gen(&shared_peer).await;

                // Detect peer swap: unconditionally reset video loop state.
                // This is the critical fix: the old pattern relied on detecting
                // an is_connected() false->true transition, which can be missed
                // if the swap + reconnect happens between loop iterations.
                if peer_gen != current_peer_gen {
                    if current_peer_gen != 0 {
                        info!(
                            old_peer_gen = current_peer_gen, new_peer_gen = peer_gen,
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
                        debug!(dropped_count, peer_gen, "Dropping video frame (not connected)");
                    }
                    continue;
                }

                // Force IDR keyframe on first connected frame so the browser
                // decoder can initialize.
                if !was_connected {
                    info!(dropped_before_connect = dropped_count, peer_gen, "WebRTC connected, forcing IDR keyframe");
                    force_keyframe.store(true, Ordering::Relaxed);
                    was_connected = true;
                    waiting_for_idr = true;
                    idr_wait_start = Instant::now();
                    idr_wait_attempts = 0;
                    error_count = 0;
                    frames_since_health_check = 0;
                    last_health_check = Instant::now();
                    // Don't `continue` here — the current frame may already be
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
                                    let _ = cmd_tx_for_video.send(CaptureCommand::ResetEncoder);
                                    idr_wait_start = Instant::now();
                                    idr_wait_attempts = 0;
                                } else {
                                    error!(
                                        resets = encoder_reset_count, peer_gen,
                                        "Exhausted encoder resets, proceeding with P-frames"
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
                    let hex_preview: String = data.iter().take(32).map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ");
                    debug!(size = data_len, is_idr, hex = %hex_preview, "Frame NAL bytes (pre-write)");
                }
                match current_peer.write_video_sample(data, frame_duration.as_nanos() as u64).await {
                    Ok(()) => {
                        video_frame_count += 1;
                        frames_since_health_check += 1;
                        if video_frame_count <= 5 {
                            info!(size = data_len, is_idr, frame = video_frame_count, peer_gen, "Video frame written to WebRTC");
                        }
                        if video_frame_count.is_multiple_of(300) {
                            info!(video_frame_count, peer_gen, "Video frames written to WebRTC");
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
                                debug!(packets, frames_since_health_check, "Video health check passed");
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
        } => {}

        // Write encoded audio frames to WebRTC
        _ = async {
            let audio_duration_ns = Duration::from_millis(20).as_nanos() as u64;
            while let Some(data) = audio_rx.recv().await {
                let current_peer = peer::snapshot(&shared_peer).await;
                if let Err(e) = current_peer.write_audio_sample(&data, audio_duration_ns).await {
                    debug!("Write audio sample: {e:#}");
                }
            }
            info!("Audio frame channel closed");
        } => {}

        // Handle signaling WebSocket
        _ = run_signaling(
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
        _ = async {
            while let Some(()) = clipboard_read_rx.recv().await {
                // Brief delay so the X11 app has time to write to the clipboard
                tokio::time::sleep(Duration::from_millis(100)).await;
                let text = {
                    let cb = clipboard_for_sync.lock().unwrap_or_else(|e| e.into_inner());
                    cb.get_text()
                };
                match text {
                    Ok(Some(ref text)) if !text.is_empty() => {
                        const MAX_CLIPBOARD_BYTES: usize = 1_048_576;
                        if text.len() <= MAX_CLIPBOARD_BYTES {
                            let msg = serde_json::json!({ "t": "c", "text": text }).to_string();
                            let current_peer = peer::snapshot(&shared_peer).await;
                            if let Err(e) = current_peer.send_data_channel_message(&msg).await {
                                warn!("Failed to send clipboard to browser: {e:#}");
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
        } => {}

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
        // NVIDIA: disabled at runtime — nvh264enc on ARM64 corrupts colors
        // when bitrate is changed dynamically via set_property.
        _ = async {
            let mut current_bitrate = config_bitrate;
            let min_bitrate: u32 = config_min_bitrate;
            let max_bitrate: u32 = config_max_bitrate;
            let abr_enabled = !matches!(encoder_type, encoder::EncoderType::Nvidia);
            if !abr_enabled {
                info!("Adaptive bitrate disabled for NVIDIA encoder (runtime changes cause color corruption)");
            }
            let mut loss_ema: f64 = 0.0;
            let mut prev_packets_sent: u64 = 0;
            let mut prev_packets_lost: i64 = 0;

            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;

                let current_peer = peer::snapshot(&shared_peer).await;
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
                    let _ = cmd_tx_for_abr.send(CaptureCommand::SetBitrate(new_bitrate));
                    current_bitrate = new_bitrate;
                }
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

/// Shared context for signaling WebSocket connection.
struct SignalingCtx<'a> {
    server_url: &'a str,
    session_id: Uuid,
    agent_token: Option<&'a str>,
    tls_cert_path: Option<&'a str>,
    shared_peer: &'a SharedPeer,
    peer_config: &'a PeerConfig,
    signal_tx: &'a mpsc::Sender<SignalingMessage>,
    force_keyframe: Arc<AtomicBool>,
    input_callback: Arc<dyn Fn(InputEvent) + Send + Sync>,
    capture_cmd_tx: &'a std::sync::mpsc::Sender<CaptureCommand>,
}

async fn run_signaling(ctx: &SignalingCtx<'_>, signal_rx: &mut mpsc::Receiver<SignalingMessage>) {
    if ctx.server_url.is_empty() {
        info!("No server URL provided, waiting for signaling via stdin or other mechanism");
        while let Some(msg) = signal_rx.recv().await {
            debug!(?msg, "Outgoing signal (no server connected)");
        }
        return;
    }

    // Connect to WebSocket with exponential backoff retry
    let mut backoff = Duration::from_secs(2);
    let max_backoff = Duration::from_secs(60);
    loop {
        info!(url = ctx.server_url, "Connecting to signaling server");

        match connect_and_handle(ctx, signal_rx).await {
            Ok(()) => {
                info!("Signaling connection closed cleanly");
                break;
            }
            Err(e) => {
                warn!("Signaling connection error: {e:#}");
                info!("Reconnecting in {} seconds...", backoff.as_secs());
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
            }
        }
    }
}

/// Build a TLS connector, pinning the server certificate if a cert path is provided.
/// Falls back to system roots if no cert path is given.
fn build_tls_connector(tls_cert_path: Option<&str>) -> tokio_tungstenite::Connector {
    let mut root_store = rustls::RootCertStore::empty();

    // Load system roots as baseline
    for cert in rustls_native_certs::load_native_certs().expect("Could not load platform certs") {
        let _ = root_store.add(cert);
    }

    // If a pinned cert PEM is provided, add it to the root store
    if let Some(cert_path) = tls_cert_path {
        match std::fs::read(cert_path) {
            Ok(pem_data) => {
                let certs: Vec<_> = rustls_pemfile::certs(&mut pem_data.as_slice())
                    .filter_map(|r| r.ok())
                    .collect();
                for cert in certs {
                    if let Err(e) = root_store.add(cert) {
                        warn!("Failed to add pinned cert to root store: {e}");
                    } else {
                        info!("Pinned server certificate from {cert_path}");
                    }
                }
            }
            Err(e) => {
                warn!(
                    "Failed to read TLS cert from {cert_path}: {e}, falling back to system roots"
                );
            }
        }
    }

    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    tokio_tungstenite::Connector::Rustls(Arc::new(tls_config))
}

async fn connect_and_handle(
    ctx: &SignalingCtx<'_>,
    signal_rx: &mut mpsc::Receiver<SignalingMessage>,
) -> anyhow::Result<()> {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;

    let url = match ctx.agent_token {
        Some(token) => format!(
            "{}/ws/agent/{}?token={}",
            ctx.server_url,
            ctx.session_id,
            urlencoding::encode(token)
        ),
        None => format!("{}/ws/agent/{}", ctx.server_url, ctx.session_id),
    };

    let connector = build_tls_connector(ctx.tls_cert_path);
    let mut ws_config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default();
    ws_config.max_message_size = Some(65_536); // 64KB max, matching server-side limit
    let (ws_stream, _) = tokio_tungstenite::connect_async_tls_with_config(
        &url,
        Some(ws_config),
        false,
        Some(connector),
    )
    .await
    .context("WebSocket connection failed")?;

    info!("Connected to signaling server");
    let (mut ws_tx, mut ws_rx) = ws_stream.split();

    // Track the last processed offer's ICE ufrag to deduplicate retried
    // offers from the browser. The browser's offer retry mechanism can
    // re-send the same SDP before the answer arrives, which previously
    // caused the agent to create multiple peers — only the last one
    // surviving, with the browser holding ICE credentials for a dead peer.
    let mut last_offer_ufrag: Option<String> = None;

    loop {
        tokio::select! {
            // Incoming messages from server
            msg = ws_rx.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<AgentCommand>(&text) {
                            Ok(AgentCommand::Signal(signal)) => {
                                match signal {
                                    SignalingMessage::Offer { sdp, .. } => {
                                        // Deduplicate: extract ICE ufrag from the SDP and
                                        // skip if we already processed this exact offer.
                                        // Browser retries re-send the same SDP (same ICE
                                        // credentials). Processing it again would destroy
                                        // the peer we just created for the first copy.
                                        let offer_ufrag = sdp.lines()
                                            .find(|l| l.starts_with("a=ice-ufrag:"))
                                            .map(|l| l.trim_start_matches("a=ice-ufrag:").to_string());
                                        if let Some(ref ufrag) = offer_ufrag
                                            && last_offer_ufrag.as_deref() == Some(ufrag) {
                                            info!(ufrag, "Ignoring duplicate SDP Offer (same ICE ufrag)");
                                            continue;
                                        }
                                        last_offer_ufrag = offer_ufrag;

                                        // Create a brand-new peer for every new SDP Offer.
                                        // This gives us fresh DTLS, ICE, SCTP, and data channel
                                        // state — the only reliable way to handle browser
                                        // reconnects (refresh, new tab, etc.).
                                        info!("Received SDP Offer — creating new WebRTC peer");
                                        let old_peer = peer::snapshot(ctx.shared_peer).await;
                                        // Close old peer (non-blocking best-effort)
                                        let _ = old_peer.close().await;

                                        match peer::create_peer(
                                            ctx.peer_config, ctx.signal_tx, ctx.session_id,
                                            &ctx.force_keyframe, Arc::clone(&ctx.input_callback),
                                        ).await {
                                            Ok(new_peer) => {
                                                // Handle the offer on the new peer
                                                match new_peer.handle_offer(&sdp).await {
                                                    Ok(answer_sdp) => {
                                                        // Swap the peer atomically
                                                        *ctx.shared_peer.write().await = new_peer;
                                                        info!("New peer installed, sending SDP answer");
                                                        let reply = SignalingMessage::Answer {
                                                            sdp: answer_sdp,
                                                            session_id: ctx.session_id,
                                                        };
                                                        let text = serde_json::to_string(&reply)?;
                                                        ws_tx.send(Message::Text(text.into())).await?;
                                                        // Recreate encoder to guarantee IDR on first frame.
                                                        // ForceKeyUnit events are unreliable on nvh264enc
                                                        // after long P-frame runs with gop-size=MAX.
                                                        let _ = ctx.capture_cmd_tx.send(CaptureCommand::ResetEncoder);
                                                        ctx.force_keyframe.store(true, Ordering::Relaxed);
                                                    }
                                                    Err(e) => {
                                                        error!("Failed to handle offer on new peer: {e:#}");
                                                    }
                                                }
                                            }
                                            Err(e) => {
                                                error!("Failed to create new peer: {e:#}");
                                            }
                                        }
                                    }
                                    SignalingMessage::IceCandidate {
                                        candidate, sdp_mid, sdp_mline_index, ..
                                    } => {
                                        let current_peer = peer::snapshot(ctx.shared_peer).await;
                                        if let Err(e) = current_peer
                                            .add_ice_candidate(&candidate, sdp_mid.as_deref(), sdp_mline_index)
                                            .await
                                        {
                                            warn!("Failed to add ICE candidate: {e:#}");
                                        }
                                    }
                                    other => {
                                        debug!(?other, "Unhandled signal message");
                                    }
                                }
                            }
                            Ok(AgentCommand::Shutdown) => {
                                info!("Received shutdown command");
                                return Ok(());
                            }
                            Err(e) => {
                                warn!("Invalid message from server: {e}");
                            }
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        return Ok(());
                    }
                    Some(Err(e)) => {
                        return Err(e.into());
                    }
                    _ => {}
                }
            }
            // Outgoing signaling messages (ICE candidates from our side)
            Some(msg) = signal_rx.recv() => {
                let text = serde_json::to_string(&msg)?;
                ws_tx.send(Message::Text(text.into())).await?;
            }
        }
    }
}
