use anyhow::{Context, Result, bail};
use std::fs;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use tracing::{debug, info, warn};

/// Minimal PulseAudio config for virtual desktop sessions.
/// Creates a null sink (virtual audio output) with a monitor source
/// that the agent can capture from.
/// Generate a PulseAudio config that binds to a display-specific socket path.
/// This avoids conflicts with any existing user-level PulseAudio instance.
fn pa_config(runtime_dir: &str) -> String {
    format!(
        "\
load-module module-null-sink sink_name=beam sink_properties=device.description=Beam
set-default-sink beam
load-module module-native-protocol-unix socket={runtime_dir}/native auth-anonymous=1
load-module module-always-sink
"
    )
}

/// Manages a virtual X display using either the dummy or nvidia video driver.
pub struct VirtualDisplay {
    display_num: u32,
    /// xrandr output name (e.g. "DUMMY0" for dummy driver, "DFP-1" for nvidia)
    output_name: String,
    xorg_child: Option<Child>,
    desktop_child: Option<Child>,
    pulse_child: Option<Child>,
    cursor_child: Option<Child>,
    /// Temp config path to clean up on drop (None for package-installed static config)
    cleanup_config: Option<String>,
    /// Temp EDID file to clean up on drop (nvidia only)
    cleanup_edid: Option<String>,
}

impl VirtualDisplay {
    /// Create and start a new virtual X display on the given display number.
    ///
    /// `gpu_driver` controls the Xorg driver: "auto" (detect), "nvidia" (force), "dummy" (force).
    /// `display_start` is needed for multi-GPU DFP output allocation.
    pub fn start(
        display_num: u32,
        width: u32,
        height: u32,
        gpu_driver: &str,
        display_start: u32,
    ) -> Result<Self> {
        let gpu_config = crate::gpu::detect_gpu(gpu_driver, display_num, display_start);

        let (config_path, cleanup_edid) = if gpu_config.driver == "nvidia" {
            // NVIDIA: generate config dynamically (needs BusID, DFP, EDID path).
            // Config and EDID must be in /etc/X11/beam/ so the Xorg setuid wrapper
            // can use them (elevated privileges require configs in /etc/X11/).
            let beam_conf_dir = "/etc/X11/beam";
            let _ = fs::create_dir_all(beam_conf_dir);

            let bus_id = gpu_config.bus_id.as_deref().unwrap_or("PCI:0:0:0");
            let dfp_output = gpu_config.dfp_output.as_deref().unwrap_or("DFP-1");
            let edid_path = format!("{beam_conf_dir}/beam-edid-{display_num}.bin");
            crate::gpu::write_edid_file_to(&edid_path)?;
            let config = generate_nvidia_xorg_config(bus_id, dfp_output, &edid_path);
            let config_path = format!("{beam_conf_dir}/beam-xorg-{display_num}.conf");
            let _ = fs::remove_file(&config_path);
            fs::write(&config_path, &config)
                .with_context(|| format!("Failed to write nvidia Xorg config to {config_path}"))?;
            info!(
                bus_id,
                dfp_output, config_path, "Using NVIDIA GPU driver for display :{display_num}"
            );
            (config_path, Some(edid_path))
        } else {
            // Dummy driver: use static package config or generate temp config
            let static_config = String::from("/etc/X11/beam-xorg.conf");
            if std::path::Path::new(&static_config).exists() {
                (static_config, None)
            } else {
                let tmp_config_path = tmp_xorg_config_path(display_num);
                let _ = fs::remove_file(&tmp_config_path);
                let config = generate_xorg_config(width, height);
                fs::write(&tmp_config_path, &config)
                    .with_context(|| format!("Failed to write Xorg config to {tmp_config_path}"))?;
                (tmp_config_path, None)
            }
        };

        Self::start_with_config(display_num, width, height, config_path, cleanup_edid)
    }

    fn start_with_config(
        display_num: u32,
        width: u32,
        height: u32,
        config_path: String,
        cleanup_edid: Option<String>,
    ) -> Result<Self> {
        let display_str = format!(":{display_num}");

        // Determine how to invoke Xorg based on config location.
        // Package installs: config in /etc/X11/, use Xorg wrapper (setuid) with
        // relative path. Xwrapper.config has allowed_users=anybody +
        // needs_root_rights=yes so Xorg can access /dev/tty0 for VT management.
        // Dev/source installs: config in /tmp, use Xorg binary directly with
        // absolute path (no elevated privilege restrictions).
        let direct_xorg_exists = std::path::Path::new("/usr/lib/xorg/Xorg").exists();
        let (xorg_bin_owned, config_arg_owned) =
            resolve_xorg_invocation(&config_path, direct_xorg_exists);
        let xorg_bin = xorg_bin_owned.as_str();

        // Capture Xorg stderr to diagnose startup failures
        let xorg_log_path = xorg_stderr_log_path(display_num);
        let xorg_log = std::fs::File::create(&xorg_log_path).ok();

        let mut child = Command::new(xorg_bin)
            .arg(&display_str)
            .arg("-config")
            .arg(&config_arg_owned)
            .arg("-noreset")
            .arg("-novtswitch")
            .arg("-nolisten")
            .arg("tcp")
            .stdout(Stdio::null())
            .stderr(xorg_log.map(Stdio::from).unwrap_or_else(Stdio::null))
            .spawn()
            .with_context(|| format!("Failed to start Xorg on {display_str}"))?;

        let pid = child.id();
        info!(display = display_num, pid, "Virtual X display started");

        // Wait briefly for Xorg to initialize
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Verify the display is running (check if process exited early)
        match child.try_wait() {
            Ok(Some(status)) => {
                // Read Xorg stderr for diagnosis
                if let Ok(stderr) = fs::read_to_string(&xorg_log_path)
                    && !stderr.is_empty()
                {
                    tracing::error!("Xorg stderr output:\n{stderr}");
                }
                bail!("Xorg exited immediately with status: {status} on :{display_num}");
            }
            Ok(None) => {} // still running, good
            Err(e) => {
                warn!("Could not check Xorg status: {e}");
            }
        }

        if !is_display_running(display_num) {
            bail!("Xorg failed to start on :{display_num}");
        }

        // Detect the xrandr output name (e.g. "DUMMY0" or "DFP-1")
        let output_name = detect_xrandr_output(&display_str);
        info!(display = display_num, output_name, "Detected xrandr output");

        // When using the static package config (no per-session modeline),
        // set the requested resolution via xrandr after Xorg starts.
        if config_path == "/etc/X11/beam-xorg.conf"
            && let Err(e) = set_display_resolution(&display_str, width, height, &output_name)
        {
            warn!("Failed to set initial resolution {width}x{height}: {e}");
        }

        // Ensure the X server uses `evdev` XKB rules. The agent injects keys
        // via XTEST using evdev scancodes + 8, which only produces correct
        // keysyms under the `evdev` ruleset. Some distros default to `base`
        // rules where the keycode→keysym mapping differs (e.g., keycode 111
        // = Print instead of Up), causing incorrect key injection.
        let _ = Command::new("setxkbmap")
            .env("DISPLAY", &display_str)
            .args(["-rules", "evdev", "-model", "pc105", "-layout", "us"])
            .output();

        // Only delete temp configs on drop, not the static package config
        let cleanup_config = if xorg_config_needs_cleanup(&config_path) {
            Some(config_path)
        } else {
            None
        };

        Ok(Self {
            display_num,
            output_name,
            xorg_child: Some(child),
            desktop_child: None,
            pulse_child: None,
            cursor_child: None,
            cleanup_config,
            cleanup_edid,
        })
    }

    /// Get the xrandr output name (e.g. "DUMMY0" or "DFP-1").
    pub fn output_name(&self) -> &str {
        &self.output_name
    }

    /// Change the resolution of the virtual display using xrandr.
    #[allow(dead_code)]
    pub fn set_resolution(&self, width: u32, height: u32) -> Result<()> {
        set_display_resolution(
            &format!(":{}", self.display_num),
            width,
            height,
            &self.output_name,
        )
    }

    /// Start a desktop environment on this display.
    /// Prefers XFCE4 for a full desktop experience. Disables the xfwm4
    /// compositor to minimize latency for remote desktop streaming.
    /// Falls back to openbox (lightweight WM) if XFCE4 is unavailable.
    pub fn start_desktop(&mut self) -> Result<()> {
        let display = format!(":{}", self.display_num);

        // Prefer XFCE4: full desktop with panels, file manager, app menu.
        if which_exists("xfce4-session") {
            let (xfce_config_dir, is_first_session) = ensure_persistent_config(self.display_num);

            // Detect default browser/terminal (every session, needed for env vars).
            let detected_browser = find_non_snap_app(&[
                "firefox-esr",
                "google-chrome-stable",
                "google-chrome",
                "chromium-browser",
                "firefox",
                "chromium",
                "epiphany-browser",
            ]);
            let detected_terminal =
                find_non_snap_app(&["xfce4-terminal", "gnome-terminal", "xterm"]);

            if let Some(browser) = detected_browser {
                info!(browser, "Default browser");
            } else {
                warn!(
                    "No non-snap browser found. Install a .deb browser: \
                     sudo apt install epiphany-browser"
                );
            }
            if let Some(term) = detected_terminal {
                info!(term, "Default terminal");
            }

            // Create XDG_RUNTIME_DIR for this session. Without it, D-Bus services,
            // GVFS, and PulseAudio can't find proper socket paths. Normally created
            // by logind for interactive sessions, but beam-agent is spawned by the
            // beam-server systemd service (not a PAM login session).
            let runtime_dir = xdg_runtime_dir(self.display_num);
            let _ = fs::remove_dir_all(&runtime_dir);
            fs::create_dir_all(&runtime_dir)
                .with_context(|| format!("Failed to create runtime dir: {runtime_dir}"))?;
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&runtime_dir, fs::Permissions::from_mode(0o700));
            }

            let pulse_server = pulse_server_socket_url(self.display_num);
            let mut cmd = Command::new("/usr/bin/dbus-launch");
            cmd.arg("--exit-with-session")
                .arg("xfce4-session")
                .env("DISPLAY", &display)
                .env("PULSE_SERVER", &pulse_server)
                .env("XDG_CONFIG_HOME", &xfce_config_dir)
                .env("XDG_RUNTIME_DIR", &runtime_dir)
                .env("XDG_CURRENT_DESKTOP", "XFCE")
                .env("XDG_SESSION_DESKTOP", "xfce")
                .env("GVFS_DISABLE_FUSE", "1");

            // Set env vars as universal fallback for apps that check directly.
            if let Some(browser) = detected_browser {
                cmd.env("BROWSER", browser);
            }
            if let Some(term) = detected_terminal {
                cmd.env("TERMINAL", term);
            }

            let child = unsafe {
                cmd.stdout(Stdio::null())
                    .stderr(Stdio::null())
                    // Create a new session (process group) so we can kill all
                    // grandchildren (xfwm4, xfce4-panel, etc.) on cleanup.
                    .pre_exec(|| {
                        if libc::setsid() == -1 {
                            return Err(std::io::Error::last_os_error());
                        }
                        Ok(())
                    })
                    .spawn()
                    .context("Failed to start XFCE4 desktop via dbus-launch")?
            };

            info!(
                display = self.display_num,
                pid = child.id(),
                "XFCE4 desktop started"
            );

            self.desktop_child = Some(child);

            // Background thread: start gnome-keyring on the session bus, and
            // on first session apply xfconf settings (compositor off, theme, etc.).
            // Subsequent sessions reuse persistent config from ~/.local/share/beam/.
            let display_for_xfconf = display.clone();
            std::thread::spawn(move || {
                // Poll for xfce4-panel to start (it needs xfwm4, xfdesktop first).
                // On fresh sessions with PAM/logind setup, XFCE can take 5-10s.
                let mut dbus_addr = None;
                for attempt in 1..=15 {
                    std::thread::sleep(std::time::Duration::from_secs(1));
                    dbus_addr = find_dbus_address_for_display(&display_for_xfconf);
                    if dbus_addr.is_some() {
                        debug!("Found DBUS session bus after {attempt}s");
                        break;
                    }
                }
                if dbus_addr.is_none() {
                    warn!(
                        "Could not find DBUS session bus after 15s, xfconf settings may not apply"
                    );
                }

                // Start gnome-keyring-daemon inside the D-Bus session so it
                // registers as org.freedesktop.secrets on the session bus.
                // VS Code and other apps use libsecret to talk to this service.
                //
                // Must use --foreground + separate --control-directory because
                // --start discovers the HOST's existing daemon via the shared
                // /run/user/ control socket and reuses it (which is on a
                // different D-Bus). A fresh daemon with its own control dir
                // registers on THIS session's bus.
                if let Some(ref addr) = dbus_addr {
                    let display_num = display_for_xfconf.trim_start_matches(':');
                    let display_num_u32 = display_num.parse::<u32>().unwrap_or(0);

                    // Control socket: ephemeral per-session (Unix sockets can't
                    // live on NFS and must be unique per display).
                    let keyring_control_dir = keyring_control_dir(display_num_u32);
                    let _ = fs::remove_dir_all(&keyring_control_dir);
                    let _ = fs::create_dir_all(&keyring_control_dir);

                    // Data dir: persistent at ~/.local/share/beam/keyring/ so
                    // stored passwords survive across sessions.
                    let home = std::env::var("HOME").unwrap_or_default();
                    let keyring_data_dir = keyring_data_dir(&home);
                    let keyrings_dir = keyrings_dir(&home);
                    let _ = fs::create_dir_all(&keyrings_dir);

                    // Set the default keyring name if not already set (first session).
                    // Do NOT pre-create login.keyring: gnome-keyring uses a binary
                    // format and an empty file causes "invalid or unrecognized
                    // format" errors. The --unlock flag with empty stdin creates
                    // the keyring file in the correct format automatically.
                    let default_path = format!("{keyrings_dir}/default");
                    if !std::path::Path::new(&default_path).exists() {
                        let _ = fs::write(&default_path, "login");
                    }

                    // Use a shell pipe to reliably deliver the empty password
                    // to --unlock via stdin. Direct Stdio::piped() + drop has
                    // a race condition with --foreground (daemon may not have
                    // started reading stdin when we close the pipe).
                    let keyring_cmd = format!(
                        "echo '' | gnome-keyring-daemon --foreground --unlock \
                         --components=secrets --control-directory={}",
                        keyring_control_dir
                    );
                    match Command::new("sh")
                        .args(["-c", &keyring_cmd])
                        .env("DISPLAY", &display_for_xfconf)
                        .env("DBUS_SESSION_BUS_ADDRESS", addr)
                        .env("XDG_DATA_HOME", &keyring_data_dir)
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn()
                    {
                        Ok(child) => {
                            info!(
                                pid = child.id(),
                                "gnome-keyring-daemon started (secrets) on session bus"
                            );
                        }
                        Err(e) => {
                            warn!("Failed to start gnome-keyring-daemon: {e}");
                        }
                    }
                }

                // On first session, apply settings via xfconf-query to ensure
                // they take effect (xfconfd may override pre-seeded XML on startup).
                // On subsequent sessions, persistent config already has the right
                // values (including any user customizations), so skip this entirely.
                if is_first_session {
                    let settings: Vec<(&str, &str, &str, &str)> = vec![
                        // Disable compositor (biggest latency offender)
                        ("xfwm4", "/general/use_compositing", "bool", "false"),
                        // Disable workspace zoom animation
                        ("xfwm4", "/general/zoom_desktop", "bool", "false"),
                        // Full opacity during move/resize (no transparency)
                        ("xfwm4", "/general/popup_opacity", "int", "100"),
                        ("xfwm4", "/general/move_opacity", "int", "100"),
                        ("xfwm4", "/general/resize_opacity", "int", "100"),
                        // Disable GTK animations (menu fade-in/out ~200ms)
                        ("xsettings", "/Net/EnableAnimations", "bool", "false"),
                        // Zero delay on submenu popup/popdown (~225ms each)
                        ("xsettings", "/Gtk/MenuPopupDelay", "int", "0"),
                        ("xsettings", "/Gtk/MenuPopdownDelay", "int", "0"),
                        // Disable cursor blink (saves encode bandwidth)
                        ("xsettings", "/Gtk/CursorBlink", "bool", "false"),
                        // Arc-Dark: modern flat dark theme, well-maintained
                        ("xsettings", "/Net/ThemeName", "string", "Arc-Dark"),
                        // Papirus-Dark: comprehensive modern icon theme
                        ("xsettings", "/Net/IconThemeName", "string", "Papirus-Dark"),
                        // Match window manager theme
                        ("xfwm4", "/general/theme", "string", "Arc-Dark"),
                        // Disable screenshooter shortcuts (beam has its own screenshot,
                        // and xfce4-screenshooter may not be installed)
                        (
                            "xfce4-keyboard-shortcuts",
                            "/commands/custom/Print",
                            "string",
                            "",
                        ),
                        (
                            "xfce4-keyboard-shortcuts",
                            "/commands/custom/<Alt>Print",
                            "string",
                            "",
                        ),
                        (
                            "xfce4-keyboard-shortcuts",
                            "/commands/custom/<Shift>Print",
                            "string",
                            "",
                        ),
                    ];

                    for (channel, prop, typ, value) in settings {
                        let mut cmd = Command::new("xfconf-query");
                        cmd.env("DISPLAY", &display_for_xfconf)
                            .args(["-c", channel, "-p", prop, "-n", "-t", typ, "-s", value]);
                        if let Some(ref addr) = dbus_addr {
                            cmd.env("DBUS_SESSION_BUS_ADDRESS", addr);
                        }
                        match cmd.output() {
                            Ok(output) if output.status.success() => {
                                debug!(channel, prop, value, "xfconf setting applied");
                            }
                            Ok(output) => {
                                let stderr = String::from_utf8_lossy(&output.stderr);
                                warn!(channel, prop, "xfconf-query failed: {stderr}");
                            }
                            Err(e) => {
                                warn!(channel, prop, "Failed to run xfconf-query: {e}");
                            }
                        }
                    }

                    info!("XFCE settings applied (compositor off, animations off, theme)");
                }
            });

            return Ok(());
        }

        // Fallback: openbox minimal WM
        if which_exists("openbox") {
            let child = Command::new("openbox")
                .env("DISPLAY", &display)
                .env("PULSE_SERVER", pulse_server_socket_url(self.display_num))
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("Failed to start openbox")?;

            info!(
                display = self.display_num,
                pid = child.id(),
                "Openbox window manager started (XFCE4 not available)"
            );

            self.desktop_child = Some(child);

            let _ = Command::new("xsetroot")
                .env("DISPLAY", &display)
                .args(["-solid", "#2d3436"])
                .output();

            // Launch a terminal so the user has something to interact with
            if which_exists("xfce4-terminal") {
                let _ = Command::new("xfce4-terminal")
                    .env("DISPLAY", &display)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
            } else if which_exists("xterm") {
                let _ = Command::new("xterm")
                    .env("DISPLAY", &display)
                    .args([
                        "-geometry",
                        "100x35+100+100",
                        "-fa",
                        "Monospace",
                        "-fs",
                        "14",
                    ])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
            }

            return Ok(());
        }

        bail!("No desktop environment found. Install xfce4 or openbox.");
    }

    /// Hide the X cursor on the virtual display so only the browser's
    /// native cursor is visible. This gives zero-latency mouse feedback
    /// since the local cursor moves instantly while the remote desktop
    /// content follows with slight network delay.
    ///
    /// Uses `unclutter` if available (best-effort, degrades gracefully).
    pub fn hide_cursor(&mut self) {
        let display = format!(":{}", self.display_num);

        // Prefer unclutter-xfixes: uses XFixes extension to set a transparent
        // cursor image. Unlike classic unclutter (which creates overlay windows
        // or changes cursor shapes), xfixes does NOT generate synthetic
        // Enter/Leave X events. This prevents hover detection issues in apps
        // like YouTube where rapid Enter/Leave causes UI overlay flicker.
        if which_exists("unclutter-xfixes") {
            match Command::new("unclutter-xfixes")
                .args(["--timeout", "0"])
                .env("DISPLAY", &display)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => {
                    info!(
                        display = self.display_num,
                        pid = child.id(),
                        "Cursor hidden via unclutter-xfixes"
                    );
                    self.cursor_child = Some(child);
                    return;
                }
                Err(e) => {
                    warn!("Failed to start unclutter-xfixes: {e}");
                }
            }
        }

        // Fallback to classic unclutter with a 1s idle timeout.
        // Using -idle 0 is too aggressive and causes synthetic Enter/Leave
        // events that break hover detection in web apps.
        if which_exists("unclutter") {
            match Command::new("unclutter")
                .args(["-idle", "1", "-root"])
                .env("DISPLAY", &display)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
            {
                Ok(child) => {
                    info!(
                        display = self.display_num,
                        pid = child.id(),
                        "Cursor hidden via unclutter (classic fallback)"
                    );
                    self.cursor_child = Some(child);
                }
                Err(e) => {
                    warn!("Failed to start unclutter: {e}");
                }
            }
        } else {
            debug!("No unclutter variant available, remote cursor will be visible");
        }
    }

    /// Start a PulseAudio daemon for this display's user session.
    pub fn start_pulseaudio(&mut self) -> Result<()> {
        let runtime_dir = pulse_runtime_dir(self.display_num);
        // Remove stale directory from previous sessions (may be owned by different user)
        let _ = fs::remove_dir_all(&runtime_dir);
        fs::create_dir_all(&runtime_dir)
            .with_context(|| format!("Failed to create PulseAudio dir: {runtime_dir}"))?;

        // Write a minimal PulseAudio config for virtual sessions.
        // Explicit socket path avoids conflict with user's existing PulseAudio.
        let pa_config_path = pulse_config_path(self.display_num);
        fs::write(&pa_config_path, pa_config(&runtime_dir))
            .with_context(|| format!("Failed to write PA config to {pa_config_path}"))?;

        let child = Command::new("pulseaudio")
            .arg("--daemonize=no")
            .arg("--exit-idle-time=-1")
            .arg("-n") // Skip default.pa — only load modules from our -F script
            .arg("-F")
            .arg(&pa_config_path)
            // Fully isolate from user's existing PulseAudio instance:
            // - PULSE_RUNTIME_PATH: where our socket + pid file go
            // - PULSE_STATE_PATH: where state database goes
            // - XDG_RUNTIME_DIR: prevents discovery of existing PA via /run/user/<uid>/pulse/
            // - Remove DBUS_SESSION_BUS_ADDRESS: prevents "D-Bus name already taken" conflict
            .env("PULSE_RUNTIME_PATH", &runtime_dir)
            .env("PULSE_STATE_PATH", &runtime_dir)
            .env("XDG_RUNTIME_DIR", &runtime_dir)
            .env_remove("DBUS_SESSION_BUS_ADDRESS")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("Failed to start PulseAudio")?;

        info!(
            display = self.display_num,
            pid = child.id(),
            "PulseAudio started"
        );

        self.pulse_child = Some(child);
        Ok(())
    }
}

impl Drop for VirtualDisplay {
    fn drop(&mut self) {
        /// Gracefully stop a child process: check if still running before
        /// sending SIGTERM to avoid killing an unrelated process if the
        /// PID has been recycled.
        fn stop_child(child: &mut Child, name: &str, display_num: u32) {
            match child.try_wait() {
                Ok(Some(_)) => return, // already exited
                Ok(None) => {}         // still running
                Err(_) => return,
            }
            let pid = child.id();
            debug!(display = display_num, pid, name, "Stopping process");
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
            let _ = child.wait();
        }

        /// Stop a desktop process group: sends SIGTERM to the entire process
        /// group (negative PID) to reach grandchildren (xfwm4, xfce4-panel,
        /// etc.) spawned by dbus-launch -> xfce4-session. Falls back to
        /// SIGKILL after a brief wait if processes are still alive.
        fn stop_desktop_group(child: &mut Child, display_num: u32) {
            match child.try_wait() {
                Ok(Some(_)) => return, // already exited
                Ok(None) => {}         // still running
                Err(_) => return,
            }
            let pid = child.id() as i32;
            debug!(display = display_num, pid, "Stopping desktop process group");
            // Send SIGTERM to the entire process group (negative PID)
            unsafe {
                libc::kill(-pid, libc::SIGTERM);
            }
            // Brief wait for graceful shutdown
            std::thread::sleep(std::time::Duration::from_millis(500));
            // Check if the lead process exited
            match child.try_wait() {
                Ok(Some(_)) => (),
                Ok(None) => {
                    // Still alive — escalate to SIGKILL on the group
                    debug!(
                        display = display_num,
                        pid, "Desktop group still alive, sending SIGKILL"
                    );
                    unsafe {
                        libc::kill(-pid, libc::SIGKILL);
                    }
                    let _ = child.wait();
                }
                Err(_) => {}
            }
        }

        // Stop cursor hider
        if let Some(ref mut child) = self.cursor_child {
            stop_child(child, "unclutter", self.display_num);
        }
        // Stop PulseAudio first
        if let Some(ref mut child) = self.pulse_child {
            stop_child(child, "pulseaudio", self.display_num);
        }
        // Stop desktop environment (kill entire process group)
        if let Some(ref mut child) = self.desktop_child {
            stop_desktop_group(child, self.display_num);
        }
        // Stop Xorg
        if let Some(ref mut child) = self.xorg_child {
            stop_child(child, "xorg", self.display_num);
        }
        if let Some(ref path) = self.cleanup_config {
            let _ = fs::remove_file(path);
        }
        if let Some(ref path) = self.cleanup_edid {
            let _ = fs::remove_file(path);
        }
        // Clean up ephemeral per-session directories.
        // NOTE: XFCE config and keyring data are NOT cleaned up — they persist
        // at ~/.local/share/beam/ across sessions.
        let _ = fs::remove_dir_all(pulse_runtime_dir(self.display_num));
        let _ = fs::remove_file(pulse_config_path(self.display_num));
        let _ = fs::remove_dir_all(keyring_control_dir(self.display_num));
        let _ = fs::remove_dir_all(xdg_runtime_dir(self.display_num));
    }
}

/// Clamp and normalize resize dimensions for safe use with xrandr and H.264.
/// Returns `None` if the dimensions are out of the valid range (320..=7680, 240..=4320).
/// Otherwise clamps to `max_width`/`max_height` (0 = unlimited, default 3840x2160),
/// enforces minimum 640x480, and rounds down to even numbers (H.264 requirement).
pub fn clamp_resize_dimensions(
    w: u32,
    h: u32,
    max_width: u32,
    max_height: u32,
) -> Option<(u32, u32)> {
    // Reject clearly invalid dimensions
    if !(320..=7680).contains(&w) || !(240..=4320).contains(&h) {
        return None;
    }

    // Apply max bounds (0 = unlimited)
    let cw = if max_width > 0 { w.min(max_width) } else { w };
    let ch = if max_height > 0 { h.min(max_height) } else { h };

    // Enforce minimum usable resolution
    let cw = cw.max(640);
    let ch = ch.max(480);

    // Round down to even (H.264 encoder requirement)
    let cw = cw & !1;
    let ch = ch & !1;

    Some((cw, ch))
}

/// Change display resolution using xrandr. Standalone function that only needs
/// the X display string (e.g. ":10"), so it can be called from the capture thread
/// without owning a VirtualDisplay reference.
pub fn set_display_resolution(
    x_display: &str,
    width: u32,
    height: u32,
    output_name: &str,
) -> Result<()> {
    // Wait for X display to be connectable (xrandr can talk to it).
    // On arm64 (e.g. NVIDIA GB10), Xorg needs more than 500ms to fully
    // initialize. Without this, xrandr fails with "Can't open display".
    for attempt in 0..10 {
        let probe = Command::new("xrandr")
            .env("DISPLAY", x_display)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .output();
        match probe {
            Ok(output) if output.status.success() => break,
            _ if attempt < 9 => {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            _ => bail!("X display {x_display} not ready after 2 seconds"),
        }
    }

    let mode_name = format!("{width}x{height}");
    let modeline = generate_modeline(width, height, 60);

    // Try to add the mode (may already exist from a previous resize).
    // Log failures — these help diagnose xrandr issues.
    let newmode_output = Command::new("xrandr")
        .env("DISPLAY", x_display)
        .args(["--newmode", &mode_name])
        .args(modeline.split_whitespace())
        .output()
        .context("Failed to run xrandr --newmode")?;
    if !newmode_output.status.success() {
        let stderr = String::from_utf8_lossy(&newmode_output.stderr);
        // "already exists" is expected for repeated resizes
        if !is_benign_xrandr_stderr(&stderr) {
            warn!("xrandr --newmode {mode_name} failed: {stderr}");
        }
    }

    // Add mode to the output (may already be added)
    let addmode_output = Command::new("xrandr")
        .env("DISPLAY", x_display)
        .args(["--addmode", output_name, &mode_name])
        .output()
        .context("Failed to run xrandr --addmode")?;
    if !addmode_output.status.success() {
        let stderr = String::from_utf8_lossy(&addmode_output.stderr);
        if !is_benign_xrandr_stderr(&stderr) {
            warn!("xrandr --addmode {output_name} {mode_name} failed: {stderr}");
        }
    }

    // Switch to the new mode
    let output = Command::new("xrandr")
        .env("DISPLAY", x_display)
        .args(["--output", output_name, "--mode", &mode_name])
        .output()
        .context("Failed to run xrandr --output")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("Failed to set resolution to {mode_name}: {stderr}");
    }

    info!(
        x_display,
        width, height, "Display resolution changed via xrandr"
    );
    Ok(())
}

/// Resolve which Xorg invocation to use (binary path + config argument)
/// from a config file path. Returns `(xorg_bin, config_arg)`.
///
/// - Config under `/etc/X11/` → use the `Xorg` wrapper (setuid for VT
///   management) with the path relative to `/etc/X11/`.
/// - Config elsewhere → use the absolute path. Prefer the direct binary
///   at `/usr/lib/xorg/Xorg` when present (skips setuid restrictions).
///
/// Pure helper so the path logic can be unit-tested without spawning Xorg.
pub(crate) fn resolve_xorg_invocation(
    config_path: &str,
    direct_xorg_binary_exists: bool,
) -> (String, String) {
    if let Some(relative) = config_path.strip_prefix("/etc/X11/") {
        return ("Xorg".to_string(), relative.to_string());
    }
    let xorg_bin = if direct_xorg_binary_exists {
        "/usr/lib/xorg/Xorg".to_string()
    } else {
        "Xorg".to_string()
    };
    (xorg_bin, config_path.to_string())
}

/// Decide whether a Xorg config path is a temp config (in `/tmp/`) that
/// should be cleaned up on VirtualDisplay drop, vs. a static package
/// config (in `/etc/X11/`) that should be left alone.
pub(crate) fn xorg_config_needs_cleanup(config_path: &str) -> bool {
    config_path.starts_with("/tmp/")
}

/// Decide whether an xrandr stderr message represents a "benign" outcome
/// (the mode is already added) versus a real error worth warning about.
/// Repeated resizes routinely hit "already exists" — log spam should be
/// suppressed for that case but kept for real failures.
pub(crate) fn is_benign_xrandr_stderr(stderr: &str) -> bool {
    stderr.contains("already exists")
}

/// Build the per-display PulseAudio runtime directory path. Pure helper
/// so the path layout can be tested without spawning PulseAudio.
pub(crate) fn pulse_runtime_dir(display_num: u32) -> String {
    format!("/tmp/beam-pulse-{display_num}")
}

/// Build the per-display PulseAudio config file path.
pub(crate) fn pulse_config_path(display_num: u32) -> String {
    format!("/tmp/beam-pulse-{display_num}.pa")
}

/// Build the per-display XDG_RUNTIME_DIR path for the desktop session.
pub(crate) fn xdg_runtime_dir(display_num: u32) -> String {
    format!("/tmp/beam-run-{display_num}")
}

/// Build the per-display keyring control directory path.
pub(crate) fn keyring_control_dir(display_num: u32) -> String {
    format!("/tmp/beam-keyring-{display_num}")
}

/// Build the per-display Xorg stderr log path.
pub(crate) fn xorg_stderr_log_path(display_num: u32) -> String {
    format!("/tmp/beam-xorg-stderr-{display_num}.log")
}

/// Build the per-display tmp xorg config path used when no static
/// `/etc/X11/beam-xorg.conf` is found.
pub(crate) fn tmp_xorg_config_path(display_num: u32) -> String {
    format!("/tmp/beam-xorg-{display_num}.conf")
}

/// Build the keyring data directory under `$HOME`. The data dir is
/// persistent across sessions so stored credentials survive.
pub(crate) fn keyring_data_dir(home: &str) -> String {
    format!("{home}/.local/share/beam/keyring")
}

/// Build the keyrings subdirectory under the data directory.
pub(crate) fn keyrings_dir(home: &str) -> String {
    format!("{}/keyrings", keyring_data_dir(home))
}

/// Build the per-display PulseAudio socket URL used by start_pulseaudio +
/// child apps via `PULSE_SERVER`. Pure helper kept consistent with
/// `pulse_runtime_dir`.
pub(crate) fn pulse_server_socket_url(display_num: u32) -> String {
    format!("unix:{}/native", pulse_runtime_dir(display_num))
}

/// Pick the panel-1 plugin id based on whether whiskermenu is installed.
/// Pure helper so the choice can be tested without filesystem state.
pub(crate) fn pick_panel_plugin(has_whiskermenu: bool) -> &'static str {
    if has_whiskermenu {
        "whiskermenu"
    } else {
        "applicationsmenu"
    }
}

/// Map a browser binary name to the XFCE helper-id used by exo-open.
/// Pure helper.
pub(crate) fn browser_to_helper_id(browser: &str) -> &str {
    match browser {
        "firefox-esr" => "firefox-esr",
        "firefox" => "firefox",
        "google-chrome-stable" | "google-chrome" => "google-chrome",
        "chromium-browser" | "chromium" => "chromium",
        "epiphany-browser" => "epiphany",
        other => other,
    }
}

/// Map a browser binary name to the .desktop file name in
/// `/usr/share/applications/`. Pure helper. Returns `""` for browsers
/// that don't have a known desktop file.
pub(crate) fn browser_to_desktop_file(browser: &str) -> &'static str {
    match browser {
        "firefox-esr" => "firefox-esr.desktop",
        "firefox" => "firefox.desktop",
        "google-chrome-stable" | "google-chrome" => "google-chrome.desktop",
        "chromium-browser" | "chromium" => "chromium-browser.desktop",
        "epiphany-browser" => "org.gnome.Epiphany.desktop",
        _ => "",
    }
}

/// Build the helpers.rc content from optional detected browser + terminal.
/// Pure helper extracted from `seed_default_config` so the format is
/// directly testable.
pub(crate) fn build_helpers_rc(
    detected_terminal: Option<&str>,
    detected_browser: Option<&str>,
) -> String {
    let mut helpers_rc = String::from("[Default]\n");
    if let Some(term) = detected_terminal {
        helpers_rc.push_str(&format!("TerminalEmulator={term}\n"));
    }
    if let Some(browser) = detected_browser {
        let helper_id = browser_to_helper_id(browser);
        helpers_rc.push_str(&format!("WebBrowser={helper_id}\n"));
    }
    helpers_rc
}

/// Build the mimeapps.list content for a default browser. Returns
/// `None` when the browser doesn't map to a known desktop file (the
/// caller skips writing the file in that case).
pub(crate) fn build_mimeapps_list(browser: &str) -> Option<String> {
    let desktop_file = browser_to_desktop_file(browser);
    if desktop_file.is_empty() {
        return None;
    }
    Some(format!(
        "[Default Applications]\n\
         x-scheme-handler/http={d}\n\
         x-scheme-handler/https={d}\n\
         text/html={d}\n\
         application/xhtml+xml={d}\n",
        d = desktop_file,
    ))
}

/// Parse a null-separated `/proc/<pid>/environ` buffer for a given
/// display string. Returns `(has_display, dbus_addr)` — the X11 display
/// matched its DISPLAY=… line, and the captured `DBUS_SESSION_BUS_ADDRESS`
/// if present. Pure helper extracted from `find_dbus_address_for_display`.
pub(crate) fn parse_environ_for_dbus(environ: &[u8], x_display: &str) -> (bool, Option<String>) {
    let mut has_display = false;
    let mut dbus_addr = None;
    for var in environ.split(|&b| b == 0) {
        let var_str = String::from_utf8_lossy(var);
        if var_str == format!("DISPLAY={x_display}") {
            has_display = true;
        }
        if let Some(addr) = var_str.strip_prefix("DBUS_SESSION_BUS_ADDRESS=") {
            dbus_addr = Some(addr.to_string());
        }
    }
    (has_display, dbus_addr)
}

/// Decide whether an /proc/<pid>/environ entry matches a target display
/// and exposes a usable DBUS address. Returns the dbus address only when
/// both conditions hold. Pure helper extracted from the same callsite.
pub(crate) fn dbus_address_for_display(environ: &[u8], x_display: &str) -> Option<String> {
    let (has_display, dbus_addr) = parse_environ_for_dbus(environ, x_display);
    if has_display { dbus_addr } else { None }
}

/// Build the systemd-user DBus socket path for a given uid. Pure
/// helper around the format. Used by `find_dbus_address_for_display`.
pub(crate) fn systemd_user_bus_path(uid: u32) -> String {
    format!("/run/user/{uid}/bus")
}

/// Build the systemd-user DBus address (the `unix:path=...` form) for
/// a given uid. The path the agent passes to subprocess `DBUS_SESSION_BUS_ADDRESS`.
pub(crate) fn systemd_user_bus_address(uid: u32) -> String {
    format!("unix:path={}", systemd_user_bus_path(uid))
}

/// Parse `xrandr --query` stdout and return the first connected output
/// name (e.g. "DUMMY0", "DFP-1", "HDMI-A-1"). Returns `None` if no line
/// contains " connected" (which is a fall-through to the production
/// default of "DUMMY0").
pub(crate) fn parse_xrandr_connected_output(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        if line.contains(" connected")
            && let Some(name) = line.split_whitespace().next()
        {
            return Some(name.to_string());
        }
    }
    None
}

/// Detect the xrandr output name for a display.
/// Parses `xrandr --query` and returns the first connected output name.
/// Falls back to "DUMMY0" if detection fails.
fn detect_xrandr_output(x_display: &str) -> String {
    // Wait for xrandr to be ready (same retry logic as set_display_resolution)
    for attempt in 0..10 {
        let result = Command::new("xrandr")
            .env("DISPLAY", x_display)
            .arg("--query")
            .output();

        match result {
            Ok(o) if o.status.success() => {
                let stdout = String::from_utf8_lossy(&o.stdout);
                // Parse lines like "DUMMY0 connected primary 1920x1080+0+0"
                // or "DFP-1 connected 1920x1080+0+0"
                if let Some(name) = parse_xrandr_connected_output(&stdout) {
                    return name;
                }
                warn!(
                    x_display,
                    "No connected output found in xrandr, using DUMMY0"
                );
                return "DUMMY0".to_string();
            }
            _ if attempt < 9 => {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            _ => break,
        }
    }
    warn!(
        x_display,
        "xrandr not ready after 2s, assuming DUMMY0 output"
    );
    "DUMMY0".to_string()
}

/// Generate an Xorg config for the NVIDIA proprietary driver.
/// Uses ConnectedMonitor + CustomEDID for headless virtual display.
fn generate_nvidia_xorg_config(bus_id: &str, dfp_output: &str, edid_path: &str) -> String {
    format!(
        r#"# Beam Virtual Display - NVIDIA GPU-accelerated
# Generated dynamically by beam-agent

Section "Device"
    Identifier  "Beam NVIDIA GPU"
    Driver      "nvidia"
    BusID       "{bus_id}"
    Option      "ConnectedMonitor" "{dfp_output}"
    Option      "CustomEDID" "{dfp_output}:{edid_path}"
    Option      "AllowEmptyInitialConfiguration" "True"
EndSection

Section "Monitor"
    Identifier  "Beam Monitor"
    HorizSync   1-200
    VertRefresh 1-200
EndSection

Section "Screen"
    Identifier  "Beam Screen"
    Device      "Beam NVIDIA GPU"
    Monitor     "Beam Monitor"
    DefaultDepth 24
    SubSection "Display"
        Depth   24
    EndSubSection
EndSection

Section "ServerFlags"
    Option "AutoAddDevices" "false"
    Option "AutoEnableDevices" "false"
    Option "AutoAddGPU" "false"
    Option "DontVTSwitch" "true"
EndSection

Section "ServerLayout"
    Identifier  "Beam Layout"
    Screen      "Beam Screen"
    Option "AutoAddDevices" "false"
EndSection
"#
    )
}

fn generate_xorg_config(width: u32, height: u32) -> String {
    // The dummy driver needs a Modeline for non-standard resolutions.
    // Without it, Xorg falls back to a default mode (e.g. 2048x1536)
    // when the requested resolution isn't a recognized standard mode.
    let modeline = generate_modeline(width, height, 60);
    // Allocate enough VRAM for up to 4K (3840x2160) so dynamic resolution
    // changes via xrandr don't fail with BadMatch. The dummy driver needs
    // VideoRam >= width*height*4/1024 for the LARGEST resolution, not just
    // the initial one. 256MB covers up to 8K.
    let vram: u32 = 262_144; // 256 MB in KB
    format!(
        r#"Section "Device"
    Identifier  "Beam Virtual GPU"
    Driver      "dummy"
    VideoRam    {vram}
EndSection

Section "Monitor"
    Identifier  "Beam Monitor"
    HorizSync   1-200
    VertRefresh 1-200
    Modeline    "{width}x{height}" {modeline}
EndSection

Section "Screen"
    Identifier  "Beam Screen"
    Device      "Beam Virtual GPU"
    Monitor     "Beam Monitor"
    DefaultDepth 24
    SubSection "Display"
        Depth   24
        Virtual 7680 4320
        Modes   "{width}x{height}"
    EndSubSection
EndSection

Section "ServerFlags"
    Option "AutoAddDevices" "false"
    Option "AutoEnableDevices" "false"
    Option "DontVTSwitch" "true"
EndSection

Section "ServerLayout"
    Identifier  "Beam Layout"
    Screen      "Beam Screen"
    Option "AutoAddDevices" "false"
EndSection
"#,
    )
}

fn generate_modeline(width: u32, height: u32, refresh: u32) -> String {
    // Simplified CVT modeline calculation
    let pixel_clock = (width as f64 * height as f64 * refresh as f64) / 1_000_000.0 * 1.2;
    format!(
        "{:.2} {} {} {} {} {} {} {} {} +hsync +vsync",
        pixel_clock,
        width,
        width + 48,
        width + 48 + 32,
        width + 48 + 32 + 80,
        height,
        height + 3,
        height + 3 + 5,
        height + 3 + 5 + 25,
    )
}

fn is_display_running(display_num: u32) -> bool {
    let lock_file = format!("/tmp/.X{display_num}-lock");
    // Read PID from lock file and verify the process is actually running
    // (handles stale lock files from crashed Xorg)
    match fs::read_to_string(&lock_file) {
        Ok(contents) => {
            if let Ok(pid) = contents.trim().parse::<i32>() {
                // signal 0 checks if process exists without signaling it
                unsafe { libc::kill(pid, 0) == 0 }
            } else {
                false
            }
        }
        Err(_) => false,
    }
}

fn which_exists(program: &str) -> bool {
    Command::new("which")
        .arg(program)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Check if a binary is a snap package. Detects both direct snap binaries
/// (/snap/bin/...) and wrapper scripts at /usr/bin/ that delegate to snap.
/// Snap apps fail in Beam sessions because they require a logind session,
/// snap environment variables, and cgroup access that beam-agent doesn't have.
fn is_snap_binary(program: &str) -> bool {
    let path = Command::new("which")
        .arg(program)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|p| p.trim().to_string());

    match path {
        Some(p) if p.starts_with("/snap/") => true,
        Some(p) => {
            // Check if it's a wrapper script that invokes snap
            std::fs::read_to_string(&p)
                .map(|contents| contents.contains("/snap/bin/") || contents.contains("exec snap"))
                .unwrap_or(false)
        }
        None => false,
    }
}

/// Find the first non-snap binary from a list of candidates.
/// Snap apps fail in Beam sessions (no logind session, no snap env vars).
fn find_non_snap_app(candidates: &[&'static str]) -> Option<&'static str> {
    candidates
        .iter()
        .copied()
        .find(|name| which_exists(name) && !is_snap_binary(name))
}

/// Discover DBUS_SESSION_BUS_ADDRESS for the current session.
/// Strategy 1: systemd user bus at /run/user/<uid>/bus (fast, reliable with PAM sessions).
/// Strategy 2: fall back to scanning /proc for xfce4-panel's environ.
fn find_dbus_address_for_display(x_display: &str) -> Option<String> {
    // Strategy 1: systemd user bus (created by pam_systemd)
    let uid = nix::unistd::getuid().as_raw();
    let bus_path = systemd_user_bus_path(uid);
    if std::path::Path::new(&bus_path).exists() {
        let addr = systemd_user_bus_address(uid);
        debug!(x_display, addr, "Using systemd user bus for DBUS");
        return Some(addr);
    }

    // Strategy 2: fall back to /proc scan
    let output = Command::new("pgrep")
        .arg("-x")
        .arg("xfce4-panel")
        .output()
        .ok()?;
    let pids = String::from_utf8_lossy(&output.stdout);
    for pid_str in pids.lines() {
        let pid = pid_str.trim();
        if pid.is_empty() {
            continue;
        }
        let Ok(environ) = fs::read(format!("/proc/{pid}/environ")) else {
            continue; // Permission denied for other users' processes — skip
        };
        if let Some(addr) = dbus_address_for_display(&environ, x_display) {
            debug!(
                x_display,
                addr, "Found DBUS session address from panel process"
            );
            return Some(addr);
        }
    }
    warn!(
        x_display,
        "Could not find DBUS_SESSION_BUS_ADDRESS for display"
    );
    None
}

/// Ensure XFCE/GTK config directory exists with default settings.
/// Uses persistent storage at `~/.local/share/beam/config/` so desktop
/// customizations (theme, panel layout, stored passwords) survive across sessions.
/// Falls back to ephemeral `/tmp/beam-xfce-{display_num}` if persistent storage
/// is unavailable (e.g. NFS home unreachable).
/// Returns `(config_dir_path, is_first_session)`.
fn ensure_persistent_config(display_num: u32) -> (String, bool) {
    match try_persistent_config() {
        Ok(result) => result,
        Err(e) => {
            warn!("Persistent config unavailable, falling back to ephemeral: {e}");
            let fallback = format!("/tmp/beam-xfce-{display_num}");
            let _ = fs::remove_dir_all(&fallback);
            let _ = fs::create_dir_all(&fallback);
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = fs::set_permissions(&fallback, fs::Permissions::from_mode(0o700));
            }
            seed_default_config(&fallback);
            (fallback, true)
        }
    }
}

fn try_persistent_config() -> Result<(String, bool)> {
    let home = std::env::var("HOME").context("HOME not set")?;
    try_persistent_config_in(&home)
}

fn try_persistent_config_in(home: &str) -> Result<(String, bool)> {
    let beam_dir = format!("{home}/.local/share/beam");
    let config_dir = format!("{beam_dir}/config");
    let sentinel = format!("{beam_dir}/.initialized");

    if std::path::Path::new(&sentinel).exists() {
        return Ok((config_dir, false));
    }

    // First session: create directory structure and seed defaults
    fs::create_dir_all(&config_dir).with_context(|| format!("Failed to create {config_dir}"))?;
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&config_dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("Failed to set permissions on {config_dir}"))?;
    }

    seed_default_config(&config_dir);

    // Version file for future config migrations
    let _ = fs::write(format!("{beam_dir}/.config-version"), "1");
    // Sentinel written last — signals initialization completed successfully
    fs::write(&sentinel, "").context("Failed to write initialization sentinel")?;

    info!("Persistent desktop config initialized at {config_dir}");
    Ok((config_dir, true))
}

/// Seed all default XFCE/GTK configuration files for a fresh Beam desktop.
/// Covers: xfconf XML channels, GTK3 settings, autostart masks, default
/// browser/terminal helpers, and MIME type associations.
fn seed_default_config(config_dir: &str) {
    let xfconf_dir = format!("{config_dir}/xfce4/xfconf/xfce-perchannel-xml");
    let _ = fs::create_dir_all(&xfconf_dir);

    // xfwm4: disable compositor, workspace zoom animation, and pre-seed theme
    let _ = fs::write(
        format!("{xfconf_dir}/xfwm4.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<channel name="xfwm4" version="1.0">
  <property name="general" type="empty">
    <property name="use_compositing" type="bool" value="false"/>
    <property name="zoom_desktop" type="bool" value="false"/>
    <property name="popup_opacity" type="int" value="100"/>
    <property name="move_opacity" type="int" value="100"/>
    <property name="resize_opacity" type="int" value="100"/>
    <property name="theme" type="string" value="Arc-Dark"/>
  </property>
</channel>
"#,
    );

    // xsettings: disable GTK animations, pre-seed theme/icons/cursor settings
    let _ = fs::write(
        format!("{xfconf_dir}/xsettings.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<channel name="xsettings" version="1.0">
  <property name="Gtk" type="empty">
    <property name="MenuPopupDelay" type="int" value="0"/>
    <property name="MenuPopdownDelay" type="int" value="0"/>
    <property name="CursorBlink" type="bool" value="false"/>
  </property>
  <property name="Net" type="empty">
    <property name="EnableAnimations" type="bool" value="false"/>
    <property name="ThemeName" type="string" value="Arc-Dark"/>
    <property name="IconThemeName" type="string" value="Papirus-Dark"/>
  </property>
</channel>
"#,
    );

    // xfce4-session: no splash screen
    let _ = fs::write(
        format!("{xfconf_dir}/xfce4-session.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<channel name="xfce4-session" version="1.0">
  <property name="splash" type="empty">
    <property name="Engine" type="string" value=""/>
  </property>
</channel>
"#,
    );

    // Pre-seed panel config: use Whisker Menu (plugin-1) if available
    let panel_plugin_1 = pick_panel_plugin(which_exists("xfce4-popup-whiskermenu"));
    let _ = fs::write(
        format!("{xfconf_dir}/xfce4-panel.xml"),
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<channel name="xfce4-panel" version="1.0">
  <property name="panels" type="array">
    <value type="int" value="1"/>
    <property name="panel-1" type="empty">
      <property name="plugin-ids" type="array">
        <value type="int" value="1"/>
        <value type="int" value="2"/>
        <value type="int" value="3"/>
        <value type="int" value="4"/>
        <value type="int" value="5"/>
        <value type="int" value="6"/>
      </property>
      <property name="position" type="string" value="p=6;x=0;y=0"/>
      <property name="position-locked" type="bool" value="true"/>
      <property name="size" type="uint" value="28"/>
    </property>
  </property>
  <property name="plugins" type="empty">
    <property name="plugin-1" type="string" value="{plugin_1}"/>
    <property name="plugin-2" type="string" value="tasklist"/>
    <property name="plugin-3" type="string" value="separator">
      <property name="expand" type="bool" value="true"/>
      <property name="style" type="uint" value="0"/>
    </property>
    <property name="plugin-4" type="string" value="systray"/>
    <property name="plugin-5" type="string" value="clock"/>
    <property name="plugin-6" type="string" value="actions"/>
  </property>
</channel>
"#,
            plugin_1 = panel_plugin_1
        ),
    );

    // Desktop wallpaper: XFCE shapes SVG (clean, lightweight)
    let _ = fs::write(
        format!("{xfconf_dir}/xfce4-desktop.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<channel name="xfce4-desktop" version="1.0">
  <property name="backdrop" type="empty">
    <property name="screen0" type="empty">
      <property name="monitorDUMMY0" type="empty">
        <property name="workspace0" type="empty">
          <property name="last-image" type="string" value="/usr/share/backgrounds/xfce/xfce-shapes.svg"/>
          <property name="image-style" type="int" value="5"/>
          <property name="color-style" type="int" value="0"/>
        </property>
      </property>
    </property>
  </property>
</channel>
"#,
    );

    // Keyboard shortcuts: Alt+F2 for app finder search
    let _ = fs::write(
        format!("{xfconf_dir}/xfce4-keyboard-shortcuts.xml"),
        r#"<?xml version="1.0" encoding="UTF-8"?>
<channel name="xfce4-keyboard-shortcuts" version="1.0">
  <property name="commands" type="empty">
    <property name="custom" type="empty">
      <property name="&lt;Alt&gt;F2" type="string" value="xfce4-appfinder --collapsed"/>
    </property>
  </property>
</channel>
"#,
    );

    // GTK3 settings: disable animations, menu delays, cursor blink
    let gtk3_dir = format!("{config_dir}/gtk-3.0");
    let _ = fs::create_dir_all(&gtk3_dir);
    let _ = fs::write(
        format!("{gtk3_dir}/settings.ini"),
        "[Settings]\n\
         gtk-enable-animations=false\n\
         gtk-menu-popup-delay=0\n\
         gtk-menu-popdown-delay=0\n\
         gtk-cursor-blink=false\n",
    );

    // GTK3 CSS: kill ALL CSS transitions (GTK themes use 200ms+
    // transitions on buttons, menus, entries, hover states etc.).
    // gtk-enable-animations only affects GtkAnimation objects, NOT CSS
    // transitions — this override is required for instant menu hover.
    let _ = fs::write(
        format!("{gtk3_dir}/gtk.css"),
        "* { transition-duration: 0s !important; animation-duration: 0s !important; }\n",
    );

    // Mask autostart entries that fail or are useless in a virtual session.
    // XDG spec: user-level .desktop files in $XDG_CONFIG_HOME/autostart/
    // override system-level files in /etc/xdg/autostart/ by filename.
    let autostart_dir = format!("{config_dir}/autostart");
    let _ = fs::create_dir_all(&autostart_dir);
    for entry in [
        "update-notifier.desktop",                     // pkexec error dialogs
        "polkit-gnome-authentication-agent-1.desktop", // pkexec auth prompts
        "pulseaudio.desktop",                          // conflicts with our PulseAudio
        "tracker-miner-fs-3.desktop",                  // file indexer wastes CPU
        "snap-userd-autostart.desktop",                // snap UI daemon
        "spice-vdagent.desktop",                       // SPICE agent, not used
        "ubuntu-advantage-notification.desktop",       // Ubuntu Pro nag
        "ubuntu-report-on-upgrade.desktop",            // upgrade reporter
        "gnome-initial-setup-copy-worker.desktop",     // GNOME first-run
        "gnome-initial-setup-first-login.desktop",     // GNOME first-run
        "org.gnome.DejaDup.Monitor.desktop",           // backup monitor
        "org.gnome.Evolution-alarm-notify.desktop",    // calendar alarms
    ] {
        let _ = fs::write(
            format!("{autostart_dir}/{entry}"),
            "[Desktop Entry]\nHidden=true\n",
        );
    }

    // Configure default applications (browser + terminal).
    // helpers.rc: XFCE helper IDs for exo-open
    // mimeapps.list: XDG MIME type associations for xdg-open
    let helpers_dir = format!("{config_dir}/xfce4");
    let _ = fs::create_dir_all(&helpers_dir);

    let detected_browser = find_non_snap_app(&[
        "firefox-esr",
        "google-chrome-stable",
        "google-chrome",
        "chromium-browser",
        "firefox",
        "chromium",
        "epiphany-browser",
    ]);
    let detected_terminal = find_non_snap_app(&["xfce4-terminal", "gnome-terminal", "xterm"]);

    let helpers_rc = build_helpers_rc(detected_terminal, detected_browser);
    let _ = fs::write(format!("{helpers_dir}/helpers.rc"), &helpers_rc);

    if let Some(browser) = detected_browser
        && let Some(content) = build_mimeapps_list(browser)
    {
        let _ = fs::write(format!("{config_dir}/mimeapps.list"), content);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xorg_config_has_generous_vram_for_dynamic_resize() {
        // Even at a small initial resolution, VRAM must be large enough
        // for fullscreen (e.g. 4K). Otherwise xrandr --output fails with
        // BadMatch when the user enters fullscreen.
        let config = generate_xorg_config(800, 600);
        assert!(
            config.contains("VideoRam    262144"),
            "VRAM should be 256MB"
        );
        // Check Virtual max size is set for dynamic resolution
        assert!(
            config.contains("Virtual 7680 4320"),
            "Virtual screen should support up to 8K"
        );
    }

    #[test]
    fn xorg_config_includes_initial_modeline() {
        let config = generate_xorg_config(1920, 1080);
        assert!(config.contains("Modeline    \"1920x1080\""));
        assert!(config.contains("Modes   \"1920x1080\""));
    }

    #[test]
    fn modeline_format_is_valid() {
        let ml = generate_modeline(1920, 1080, 60);
        let parts: Vec<&str> = ml.split_whitespace().collect();
        // Should be: clock h h_sync_start h_sync_end h_total v v_sync_start v_sync_end v_total +hsync +vsync
        assert_eq!(parts.len(), 11, "Modeline should have 11 fields: {ml}");
        // Pixel clock should be positive
        let clock: f64 = parts[0].parse().expect("clock should be a float");
        assert!(clock > 0.0, "Pixel clock should be positive");
        // h_total > width
        let h_total: u32 = parts[4].parse().unwrap();
        assert!(h_total > 1920, "h_total should be > width");
        // v_total > height
        let v_total: u32 = parts[8].parse().unwrap();
        assert!(v_total > 1080, "v_total should be > height");
        // Sync flags
        assert_eq!(parts[9], "+hsync");
        assert_eq!(parts[10], "+vsync");
    }

    #[test]
    fn modeline_dimensions_are_correct() {
        let ml = generate_modeline(1800, 1168, 60);
        let parts: Vec<&str> = ml.split_whitespace().collect();
        assert_eq!(parts[1], "1800", "hdisp should match width");
        assert_eq!(parts[5], "1168", "vdisp should match height");
    }

    #[test]
    fn clamp_resize_rejects_too_small() {
        assert_eq!(clamp_resize_dimensions(100, 100, 0, 0), None);
        assert_eq!(clamp_resize_dimensions(319, 480, 0, 0), None);
        assert_eq!(clamp_resize_dimensions(640, 239, 0, 0), None);
    }

    #[test]
    fn clamp_resize_rejects_too_large() {
        assert_eq!(clamp_resize_dimensions(7681, 1080, 0, 0), None);
        assert_eq!(clamp_resize_dimensions(1920, 4321, 0, 0), None);
    }

    #[test]
    fn clamp_resize_enforces_max_bounds() {
        // max_width=1920, max_height=1080
        let (w, h) = clamp_resize_dimensions(2560, 1440, 1920, 1080).unwrap();
        assert_eq!(w, 1920);
        assert_eq!(h, 1080);
    }

    #[test]
    fn clamp_resize_unlimited_max() {
        // max=0 means unlimited
        let (w, h) = clamp_resize_dimensions(3840, 2160, 0, 0).unwrap();
        assert_eq!(w, 3840);
        assert_eq!(h, 2160);
    }

    #[test]
    fn clamp_resize_enforces_min_640x480() {
        let (w, h) = clamp_resize_dimensions(320, 240, 0, 0).unwrap();
        assert_eq!(w, 640);
        assert_eq!(h, 480);
    }

    #[test]
    fn clamp_resize_enforces_even_dimensions() {
        // Odd dimensions should be rounded down to even
        let (w, h) = clamp_resize_dimensions(1921, 1081, 0, 0).unwrap();
        assert_eq!(w, 1920);
        assert_eq!(h, 1080);
    }

    #[test]
    fn clamp_resize_passthrough_normal() {
        let (w, h) = clamp_resize_dimensions(1920, 1080, 3840, 2160).unwrap();
        assert_eq!(w, 1920);
        assert_eq!(h, 1080);
    }

    #[test]
    fn clamp_resize_even_after_max_clamp() {
        // If max bound produces an odd number, still round to even
        let (w, h) = clamp_resize_dimensions(2000, 1200, 1921, 1081).unwrap();
        assert_eq!(w, 1920);
        assert_eq!(h, 1080);
    }

    #[test]
    fn find_dbus_prefers_user_bus() {
        // If /run/user/<uid>/bus exists, the function should return it
        let uid = nix::unistd::getuid().as_raw();
        let bus_path = format!("/run/user/{uid}/bus");
        if std::path::Path::new(&bus_path).exists() {
            let result = find_dbus_address_for_display(":99");
            assert_eq!(
                result,
                Some(format!("unix:path={bus_path}")),
                "Should prefer systemd user bus when it exists"
            );
        }
        // If the bus doesn't exist, this test is a no-op (CI environments)
    }

    #[test]
    fn find_dbus_returns_none_for_nonexistent_display() {
        // Use a display number that won't have any running processes.
        // If user bus exists, it returns that regardless of display, so only
        // test the fallback behavior when user bus is absent.
        let uid = nix::unistd::getuid().as_raw();
        let bus_path = format!("/run/user/{uid}/bus");
        if !std::path::Path::new(&bus_path).exists() {
            let result = find_dbus_address_for_display(":9999");
            assert!(
                result.is_none(),
                "Should return None for nonexistent display when no user bus"
            );
        }
    }

    #[test]
    fn seed_default_config_creates_correct_structure() {
        let dir = std::env::temp_dir().join(format!("beam-test-seed-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let dir_str = dir.to_str().unwrap();

        seed_default_config(dir_str);

        // Verify xfconf XML files exist
        let xfconf = dir.join("xfce4/xfconf/xfce-perchannel-xml");
        assert!(xfconf.join("xfwm4.xml").exists(), "xfwm4.xml missing");
        assert!(
            xfconf.join("xsettings.xml").exists(),
            "xsettings.xml missing"
        );
        assert!(
            xfconf.join("xfce4-session.xml").exists(),
            "xfce4-session.xml missing"
        );
        assert!(
            xfconf.join("xfce4-panel.xml").exists(),
            "xfce4-panel.xml missing"
        );
        assert!(
            xfconf.join("xfce4-desktop.xml").exists(),
            "xfce4-desktop.xml missing"
        );
        assert!(
            xfconf.join("xfce4-keyboard-shortcuts.xml").exists(),
            "keyboard shortcuts missing"
        );

        // Verify GTK3 config
        assert!(
            dir.join("gtk-3.0/settings.ini").exists(),
            "GTK settings missing"
        );
        assert!(dir.join("gtk-3.0/gtk.css").exists(), "GTK CSS missing");

        // Verify autostart masks
        assert!(
            dir.join("autostart/pulseaudio.desktop").exists(),
            "autostart mask missing"
        );

        // Verify helpers.rc exists
        assert!(dir.join("xfce4/helpers.rc").exists(), "helpers.rc missing");

        // Verify theme settings in XML
        let xsettings = fs::read_to_string(xfconf.join("xsettings.xml")).unwrap();
        assert!(
            xsettings.contains(r#""ThemeName" type="string" value="Arc-Dark""#),
            "xsettings.xml should pre-seed Arc-Dark theme"
        );
        assert!(
            xsettings.contains(r#""IconThemeName" type="string" value="Papirus-Dark""#),
            "xsettings.xml should pre-seed Papirus-Dark icons"
        );

        let xfwm4 = fs::read_to_string(xfconf.join("xfwm4.xml")).unwrap();
        assert!(
            xfwm4.contains(r#""use_compositing" type="bool" value="false""#),
            "xfwm4.xml should disable compositor"
        );
        assert!(
            xfwm4.contains(r#""theme" type="string" value="Arc-Dark""#),
            "xfwm4.xml should pre-seed Arc-Dark window theme"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persistent_config_creates_sentinel_and_version() {
        let dir = std::env::temp_dir().join(format!("beam-test-persist-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let home = dir.to_str().unwrap();
        let beam_dir = dir.join(".local/share/beam");

        let result = try_persistent_config_in(home);
        assert!(result.is_ok(), "First call should succeed");
        let (config_dir, is_first) = result.unwrap();
        assert!(is_first, "First call should report is_first_session=true");
        assert!(
            config_dir.ends_with(".local/share/beam/config"),
            "Config dir should be under beam/"
        );

        // Verify sentinel and version
        assert!(beam_dir.join(".initialized").exists(), "Sentinel missing");
        assert!(
            beam_dir.join(".config-version").exists(),
            "Version file missing"
        );
        assert_eq!(
            fs::read_to_string(beam_dir.join(".config-version")).unwrap(),
            "1"
        );

        // Verify config files were seeded
        assert!(
            std::path::Path::new(&config_dir)
                .join("xfce4/xfconf/xfce-perchannel-xml/xfwm4.xml")
                .exists(),
            "Config files should be seeded"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persistent_config_skips_on_subsequent_call() {
        let dir = std::env::temp_dir().join(format!("beam-test-skip-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let home = dir.to_str().unwrap();

        // First call: seeds config
        let (_, is_first) = try_persistent_config_in(home).unwrap();
        assert!(is_first);

        // Modify a config file to verify it's not overwritten
        let xfwm4_path =
            dir.join(".local/share/beam/config/xfce4/xfconf/xfce-perchannel-xml/xfwm4.xml");
        fs::write(&xfwm4_path, "user-customized").unwrap();

        // Second call: should skip seeding
        let (_, is_first) = try_persistent_config_in(home).unwrap();
        assert!(
            !is_first,
            "Second call should report is_first_session=false"
        );

        // Verify user's customization was preserved
        let content = fs::read_to_string(&xfwm4_path).unwrap();
        assert_eq!(
            content, "user-customized",
            "User customizations should be preserved on subsequent sessions"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persistent_config_fallback_on_error() {
        // try_persistent_config_in should fail for a non-writable path,
        // and ensure_persistent_config should fall back to ephemeral /tmp.
        let result = try_persistent_config_in("/proc/nonexistent");
        assert!(result.is_err(), "Should fail for non-writable path");

        // Test the full fallback via ensure_persistent_config by temporarily
        // setting HOME (safe in this specific test — no concurrent HOME readers).
        let original_home = std::env::var("HOME").unwrap();
        unsafe { std::env::set_var("HOME", "/proc/nonexistent") };

        let (config_dir, is_first) = ensure_persistent_config(9999);
        assert!(is_first, "Fallback should report first session");
        assert_eq!(
            config_dir, "/tmp/beam-xfce-9999",
            "Should fall back to ephemeral path"
        );

        // Verify fallback dir was created with config files
        assert!(
            std::path::Path::new("/tmp/beam-xfce-9999/xfce4/xfconf/xfce-perchannel-xml/xfwm4.xml")
                .exists(),
            "Fallback should seed config files"
        );

        unsafe { std::env::set_var("HOME", &original_home) };
        let _ = fs::remove_dir_all("/tmp/beam-xfce-9999");
    }

    #[test]
    fn seed_default_config_writes_gtk_css_transitions() {
        let dir = std::env::temp_dir().join(format!("beam-test-gtk-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let dir_str = dir.to_str().unwrap();

        seed_default_config(dir_str);

        let css = fs::read_to_string(dir.join("gtk-3.0/gtk.css")).unwrap();
        assert!(
            css.contains("transition-duration: 0s"),
            "GTK CSS should disable transitions"
        );
        assert!(
            css.contains("animation-duration: 0s"),
            "GTK CSS should disable animations"
        );

        let settings = fs::read_to_string(dir.join("gtk-3.0/settings.ini")).unwrap();
        assert!(
            settings.contains("gtk-enable-animations=false"),
            "GTK settings should disable animations"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_default_config_masks_autostart_entries() {
        let dir = std::env::temp_dir().join(format!("beam-test-auto-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let dir_str = dir.to_str().unwrap();

        seed_default_config(dir_str);

        let autostart = dir.join("autostart");
        // Verify a sample of masked entries
        for entry in [
            "pulseaudio.desktop",
            "tracker-miner-fs-3.desktop",
            "update-notifier.desktop",
        ] {
            let path = autostart.join(entry);
            assert!(path.exists(), "{entry} should be masked");
            let content = fs::read_to_string(&path).unwrap();
            assert!(content.contains("Hidden=true"), "{entry} should be hidden");
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_default_config_writes_compositor_disabled() {
        let dir = std::env::temp_dir().join(format!("beam-test-comp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let dir_str = dir.to_str().unwrap();

        seed_default_config(dir_str);

        let xfwm4 =
            fs::read_to_string(dir.join("xfce4/xfconf/xfce-perchannel-xml/xfwm4.xml")).unwrap();
        assert!(
            xfwm4.contains(r#""use_compositing" type="bool" value="false""#),
            "Compositor should be disabled by default"
        );
        assert!(
            xfwm4.contains(r#""zoom_desktop" type="bool" value="false""#),
            "Workspace zoom should be disabled"
        );
        assert!(
            xfwm4.contains(r#""popup_opacity" type="int" value="100""#),
            "Popup opacity should be 100%"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_default_config_writes_panel_config() {
        let dir = std::env::temp_dir().join(format!("beam-test-panel-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let dir_str = dir.to_str().unwrap();

        seed_default_config(dir_str);

        let panel =
            fs::read_to_string(dir.join("xfce4/xfconf/xfce-perchannel-xml/xfce4-panel.xml"))
                .unwrap();
        assert!(
            panel.contains("tasklist"),
            "Panel should have tasklist plugin"
        );
        assert!(panel.contains("clock"), "Panel should have clock plugin");
        assert!(
            panel.contains("systray"),
            "Panel should have systray plugin"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persistent_config_sentinel_prevents_reseeding() {
        // Verify that even with a corrupt/partial config dir, the sentinel
        // prevents re-seeding (user customizations are preserved).
        let dir = std::env::temp_dir().join(format!("beam-test-sentinel-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let home = dir.to_str().unwrap();
        let beam_dir = dir.join(".local/share/beam");
        let config_dir = beam_dir.join("config");

        // First call seeds everything
        let (_, is_first) = try_persistent_config_in(home).unwrap();
        assert!(is_first);

        // Delete some config files (simulating user removing them)
        let _ = fs::remove_file(config_dir.join("gtk-3.0/gtk.css"));

        // Second call should NOT re-create deleted files
        let (_, is_first) = try_persistent_config_in(home).unwrap();
        assert!(!is_first);
        assert!(
            !config_dir.join("gtk-3.0/gtk.css").exists(),
            "Deleted file should not be re-created after sentinel exists"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn persistent_config_version_file_content() {
        let dir = std::env::temp_dir().join(format!("beam-test-ver-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let home = dir.to_str().unwrap();

        let _ = try_persistent_config_in(home).unwrap();

        let version = fs::read_to_string(dir.join(".local/share/beam/.config-version")).unwrap();
        assert_eq!(version, "1", "Config version should be 1");

        let _ = fs::remove_dir_all(&dir);
    }

    // --- pa_config ---

    #[test]
    fn pa_config_inserts_runtime_dir_into_socket_path() {
        let config = pa_config("/tmp/beam-pulse-42");
        assert!(
            config.contains("socket=/tmp/beam-pulse-42/native"),
            "pa_config must template the runtime dir into the socket path"
        );
    }

    #[test]
    fn pa_config_loads_required_modules() {
        let config = pa_config("/tmp/beam-pulse-42");
        // The 3 modules + set-default-sink line are all required.
        assert!(config.contains("module-null-sink"));
        assert!(config.contains("module-native-protocol-unix"));
        assert!(config.contains("module-always-sink"));
        assert!(config.contains("set-default-sink beam"));
    }

    #[test]
    fn pa_config_uses_auth_anonymous_for_local_socket() {
        // The agent runs in the same user namespace as PulseAudio; the socket
        // is permission-bound at the filesystem level, so auth-anonymous=1 is
        // correct (no shared-cookie dance needed).
        let config = pa_config("/tmp/beam-pulse-1");
        assert!(config.contains("auth-anonymous=1"));
    }

    #[test]
    fn pa_config_uses_beam_sink_name() {
        // The sink name "beam" is referenced by name in audio capture code;
        // changing it requires coordination with the agent's audio side.
        let config = pa_config("/tmp/beam-pulse-1");
        assert!(config.contains("sink_name=beam"));
    }

    // --- which_exists ---

    #[test]
    fn which_exists_returns_true_for_real_binary() {
        // `sh` exists on every POSIX system. If `which` itself is missing the
        // function returns false — that's an acceptable fallback.
        let result = which_exists("sh");
        // If `which` is itself missing, the test can't make claims; otherwise
        // sh must be found.
        if Command::new("which")
            .arg("sh")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
        {
            assert!(result, "sh should always be found");
        }
    }

    #[test]
    fn which_exists_returns_false_for_bogus_binary() {
        assert!(!which_exists("definitely-not-a-real-binary-xyz-123"));
    }

    // --- is_snap_binary ---

    #[test]
    fn is_snap_binary_returns_false_for_missing_binary() {
        // A binary that doesn't exist is not a snap binary (and the path
        // lookup fails). The function returns false silently.
        assert!(!is_snap_binary("definitely-not-real-binary-xyz"));
    }

    #[test]
    fn is_snap_binary_returns_false_for_sh() {
        // /bin/sh on real systems isn't under /snap/ and isn't a snap wrapper
        // script. Verify the function correctly identifies it as not-snap.
        // (If sh is missing, the test is a no-op.)
        if Command::new("which")
            .arg("sh")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
        {
            assert!(!is_snap_binary("sh"));
        }
    }

    // --- find_non_snap_app ---

    #[test]
    fn find_non_snap_app_returns_none_for_all_missing() {
        let result = find_non_snap_app(&["bogus-app-1", "bogus-app-2", "bogus-app-3"]);
        assert_eq!(result, None);
    }

    #[test]
    fn find_non_snap_app_returns_first_real_binary() {
        // sh and (probably) cat exist on every Unix; the function returns the
        // first one in the list. We use it with multiple candidates so the
        // test doesn't presume which specific binary is present.
        if Command::new("which")
            .arg("sh")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
        {
            let result = find_non_snap_app(&["sh"]);
            assert_eq!(result, Some("sh"));
        }
    }

    #[test]
    fn find_non_snap_app_skips_missing_to_reach_real() {
        if Command::new("which")
            .arg("sh")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
        {
            let result = find_non_snap_app(&["bogus-1", "bogus-2", "sh"]);
            assert_eq!(result, Some("sh"));
        }
    }

    // --- generate_nvidia_xorg_config ---

    #[test]
    fn nvidia_xorg_config_includes_bus_id() {
        let cfg = generate_nvidia_xorg_config("PCI:42:0:0", "DFP-1", "/tmp/edid.bin");
        assert!(cfg.contains("BusID       \"PCI:42:0:0\""));
    }

    #[test]
    fn nvidia_xorg_config_includes_dfp_output() {
        let cfg = generate_nvidia_xorg_config("PCI:1:0:0", "DFP-2", "/tmp/edid.bin");
        assert!(cfg.contains(r#"ConnectedMonitor" "DFP-2""#));
        assert!(cfg.contains("DFP-2:/tmp/edid.bin"));
    }

    #[test]
    fn nvidia_xorg_config_uses_nvidia_driver() {
        let cfg = generate_nvidia_xorg_config("PCI:0:0:0", "DFP-0", "/tmp/edid.bin");
        assert!(cfg.contains("Driver      \"nvidia\""));
    }

    #[test]
    fn nvidia_xorg_config_sets_24bit_depth() {
        let cfg = generate_nvidia_xorg_config("PCI:0:0:0", "DFP-0", "/tmp/edid.bin");
        assert!(cfg.contains("DefaultDepth 24"));
    }

    #[test]
    fn nvidia_xorg_config_disables_auto_devices() {
        let cfg = generate_nvidia_xorg_config("PCI:0:0:0", "DFP-0", "/tmp/edid.bin");
        // ServerFlags section must disable autodetection so Xorg uses our
        // explicit Device entry.
        assert!(cfg.contains(r#""AutoAddDevices" "false""#));
        assert!(cfg.contains(r#""AutoAddGPU" "false""#));
        assert!(cfg.contains(r#""DontVTSwitch" "true""#));
    }

    // --- is_display_running ---

    #[test]
    fn is_display_running_false_for_missing_lock_file() {
        // No lock file → display is not running.
        // Use a display number high enough that it cannot conflict with any
        // real session.
        assert!(!is_display_running(99998));
    }

    #[test]
    fn is_display_running_false_for_stale_lock_file() {
        // Write a lock file with a PID that almost certainly doesn't exist
        // (i32::MAX). The function should detect the missing process and
        // return false.
        // Use a display number unique to this test process.
        let display_num = 99999u32;
        let lock_file = format!("/tmp/.X{display_num}-lock");
        let _ = fs::remove_file(&lock_file);

        let _ = fs::write(&lock_file, "2147483646");
        assert!(!is_display_running(display_num));

        let _ = fs::remove_file(&lock_file);
    }

    #[test]
    fn is_display_running_false_for_garbage_lock_content() {
        let display_num = 99997u32;
        let lock_file = format!("/tmp/.X{display_num}-lock");
        let _ = fs::remove_file(&lock_file);

        let _ = fs::write(&lock_file, "not-a-pid");
        assert!(!is_display_running(display_num));

        let _ = fs::remove_file(&lock_file);
    }

    #[test]
    fn is_display_running_true_for_current_process_pid() {
        // Write our own PID to the lock file. The signal-0 check should
        // confirm the process exists.
        let display_num = 99996u32;
        let lock_file = format!("/tmp/.X{display_num}-lock");
        let _ = fs::remove_file(&lock_file);

        let pid = std::process::id();
        let _ = fs::write(&lock_file, pid.to_string());
        assert!(is_display_running(display_num));

        let _ = fs::remove_file(&lock_file);
    }

    // --- ensure_persistent_config: fallback when HOME points at /proc ---

    #[test]
    fn ensure_persistent_config_returns_fallback_for_unwritable_home() {
        // HOME=/proc/nonexistent → try_persistent_config_in fails → fallback
        // to /tmp/beam-xfce-<num>.
        let original_home = std::env::var("HOME").unwrap();
        unsafe { std::env::set_var("HOME", "/proc/nonexistent-12345") };

        let (path, is_first) = ensure_persistent_config(99995);
        assert!(is_first, "Fallback should report first session");
        assert_eq!(path, "/tmp/beam-xfce-99995");
        // Cleanup
        let _ = fs::remove_dir_all(&path);

        unsafe { std::env::set_var("HOME", &original_home) };
    }

    // --- generate_modeline: invariants ---

    #[test]
    fn modeline_pixel_clock_scales_with_refresh() {
        // Pixel clock = width * height * refresh / 1M * 1.2. At 30Hz it's
        // exactly half what it is at 60Hz.
        let ml30 = generate_modeline(1920, 1080, 30);
        let ml60 = generate_modeline(1920, 1080, 60);
        let clock30: f64 = ml30.split_whitespace().next().unwrap().parse().unwrap();
        let clock60: f64 = ml60.split_whitespace().next().unwrap().parse().unwrap();
        // Allow a small float tolerance.
        assert!(
            (clock60 - clock30 * 2.0).abs() < 0.01,
            "60Hz clock should be exactly 2x 30Hz clock"
        );
    }

    #[test]
    fn modeline_high_resolution_does_not_overflow() {
        // 7680x4320 @ 120Hz — biggest sane resolution. Make sure no overflow
        // and the structure is still well-formed.
        let ml = generate_modeline(7680, 4320, 120);
        let parts: Vec<&str> = ml.split_whitespace().collect();
        assert_eq!(parts.len(), 11);
        let clock: f64 = parts[0].parse().unwrap();
        assert!(
            clock > 0.0 && clock.is_finite(),
            "Pixel clock must be a finite positive number"
        );
    }

    // --- xorg config: dummy driver section ---

    #[test]
    fn xorg_config_uses_dummy_driver() {
        let cfg = generate_xorg_config(1920, 1080);
        assert!(cfg.contains("Driver      \"dummy\""));
    }

    #[test]
    fn xorg_config_locks_auto_devices_disabled() {
        // ServerFlags must keep AutoAddDevices off to prevent Xorg from
        // grabbing real input devices on hosts with a physical keyboard.
        let cfg = generate_xorg_config(800, 600);
        assert!(cfg.contains(r#""AutoAddDevices" "false""#));
        assert!(cfg.contains(r#""DontVTSwitch" "true""#));
    }

    // --- clamp_resize_dimensions: exhaustive boundary tests ---

    #[test]
    fn clamp_resize_minimum_valid_input() {
        // 320x240 is on the lower boundary of the validation window.
        // It must pass through and be clamped UP to the 640x480 floor.
        let (w, h) = clamp_resize_dimensions(320, 240, 0, 0).unwrap();
        assert!(w >= 640);
        assert!(h >= 480);
    }

    #[test]
    fn clamp_resize_maximum_valid_input() {
        let (w, h) = clamp_resize_dimensions(7680, 4320, 0, 0).unwrap();
        assert_eq!(w, 7680);
        assert_eq!(h, 4320);
    }

    #[test]
    fn clamp_resize_lower_width_boundary() {
        // Exactly 320 passes; 319 fails.
        assert!(clamp_resize_dimensions(320, 480, 0, 0).is_some());
        assert!(clamp_resize_dimensions(319, 480, 0, 0).is_none());
    }

    #[test]
    fn clamp_resize_upper_width_boundary() {
        // Exactly 7680 passes; 7681 fails.
        assert!(clamp_resize_dimensions(7680, 1080, 0, 0).is_some());
        assert!(clamp_resize_dimensions(7681, 1080, 0, 0).is_none());
    }

    #[test]
    fn clamp_resize_lower_height_boundary() {
        // Exactly 240 passes; 239 fails.
        assert!(clamp_resize_dimensions(640, 240, 0, 0).is_some());
        assert!(clamp_resize_dimensions(640, 239, 0, 0).is_none());
    }

    #[test]
    fn clamp_resize_upper_height_boundary() {
        assert!(clamp_resize_dimensions(1920, 4320, 0, 0).is_some());
        assert!(clamp_resize_dimensions(1920, 4321, 0, 0).is_none());
    }

    #[test]
    fn clamp_resize_max_zero_is_unlimited() {
        // max=0 must NOT clamp anything; only the absolute validation window
        // matters.
        let (w, h) = clamp_resize_dimensions(5000, 3000, 0, 0).unwrap();
        assert_eq!(w, 5000);
        assert_eq!(h, 3000);
    }

    #[test]
    fn clamp_resize_partial_max_constraints() {
        // Only max_width set; height passes through.
        let (w, h) = clamp_resize_dimensions(3000, 1500, 1920, 0).unwrap();
        assert_eq!(w, 1920);
        assert_eq!(h, 1500);
    }

    #[test]
    fn clamp_resize_partial_max_height_only() {
        let (w, h) = clamp_resize_dimensions(2000, 1500, 0, 1080).unwrap();
        assert_eq!(w, 2000);
        assert_eq!(h, 1080);
    }

    // --- pa_config ---

    #[test]
    fn pa_config_uses_runtime_dir_in_socket_path() {
        // Socket path must reference the runtime dir verbatim.
        let cfg = pa_config("/tmp/beam-pulse-10");
        assert!(cfg.contains("/tmp/beam-pulse-10/native"));
    }

    #[test]
    fn pa_config_contains_required_modules() {
        let cfg = pa_config("/tmp/test-runtime");
        // null-sink — virtual sink so beam-agent can capture from monitor
        assert!(cfg.contains("module-null-sink"));
        // native-protocol-unix — actual TCP-replacement socket
        assert!(cfg.contains("module-native-protocol-unix"));
        // always-sink — ensures something is always playing if not specified
        assert!(cfg.contains("module-always-sink"));
        // default sink — apps fall through to this if they don't pick one
        assert!(cfg.contains("set-default-sink beam"));
    }

    #[test]
    fn pa_config_enables_anonymous_auth() {
        // auth-anonymous=1 lets the beam-agent process talk to PulseAudio
        // without exchanging cookies — fine because the socket is in a
        // 0700 runtime dir and only beam-agent can reach it.
        let cfg = pa_config("/tmp/x");
        assert!(cfg.contains("auth-anonymous=1"));
    }

    #[test]
    fn pa_config_handles_paths_with_spaces() {
        // Defensive: even a path with unusual characters must round-trip
        // into the format! template without panicking.
        let cfg = pa_config("/tmp/beam pulse");
        assert!(cfg.contains("/tmp/beam pulse/native"));
    }

    // --- generate_nvidia_xorg_config ---

    #[test]
    fn nvidia_config_includes_bus_id() {
        let cfg = generate_nvidia_xorg_config("PCI:1:0:0", "DFP-1", "/etc/X11/beam/edid.bin");
        assert!(cfg.contains("PCI:1:0:0"));
    }

    #[test]
    fn nvidia_config_includes_dfp_output() {
        let cfg = generate_nvidia_xorg_config("PCI:0:0:0", "DFP-2", "/etc/X11/beam/edid.bin");
        assert!(cfg.contains("DFP-2"));
        // ConnectedMonitor is the NVIDIA option that names the virtual output
        assert!(cfg.contains("ConnectedMonitor"));
    }

    #[test]
    fn nvidia_config_includes_custom_edid() {
        let cfg =
            generate_nvidia_xorg_config("PCI:0:0:0", "DFP-1", "/etc/X11/beam/beam-edid-10.bin");
        assert!(cfg.contains("CustomEDID"));
        assert!(cfg.contains("/etc/X11/beam/beam-edid-10.bin"));
    }

    #[test]
    fn nvidia_config_pairs_dfp_with_edid_path() {
        // The CustomEDID syntax is "{output}:{edid_path}" — verify both ends.
        let cfg = generate_nvidia_xorg_config("PCI:0:0:0", "DFP-3", "/path/to/edid.bin");
        assert!(cfg.contains("DFP-3:/path/to/edid.bin"));
    }

    #[test]
    fn nvidia_config_uses_nvidia_driver() {
        let cfg = generate_nvidia_xorg_config("PCI:0:0:0", "DFP-1", "/edid");
        // Section "Device" must declare driver "nvidia"
        assert!(cfg.contains(r#"Driver      "nvidia""#));
    }

    #[test]
    fn nvidia_config_disables_auto_devices() {
        let cfg = generate_nvidia_xorg_config("PCI:0:0:0", "DFP-1", "/edid");
        // No keyboards/mice — beam-agent injects via XTEST only.
        assert!(cfg.contains(r#"Option "AutoAddDevices" "false""#));
        assert!(cfg.contains(r#"Option "AutoEnableDevices" "false""#));
    }

    #[test]
    fn nvidia_config_disables_vt_switching() {
        // DontVTSwitch prevents the X server from grabbing the console.
        let cfg = generate_nvidia_xorg_config("PCI:0:0:0", "DFP-1", "/edid");
        assert!(cfg.contains(r#"Option "DontVTSwitch" "true""#));
    }

    #[test]
    fn nvidia_config_allows_empty_initial_config() {
        // AllowEmptyInitialConfiguration is required for headless boot —
        // without it Xorg refuses to start if no monitor is connected.
        let cfg = generate_nvidia_xorg_config("PCI:0:0:0", "DFP-1", "/edid");
        assert!(cfg.contains(r#"Option      "AllowEmptyInitialConfiguration" "True""#));
    }

    // --- generate_xorg_config (dummy driver) ---

    #[test]
    fn dummy_xorg_config_uses_dummy_driver() {
        let cfg = generate_xorg_config(1920, 1080);
        assert!(cfg.contains(r#"Driver      "dummy""#));
    }

    #[test]
    fn dummy_xorg_config_no_auto_devices() {
        let cfg = generate_xorg_config(1280, 720);
        assert!(cfg.contains(r#"Option "AutoAddDevices" "false""#));
        assert!(cfg.contains(r#"Option "AutoEnableDevices" "false""#));
    }

    #[test]
    fn dummy_xorg_config_dont_vt_switch() {
        let cfg = generate_xorg_config(800, 600);
        assert!(cfg.contains(r#"Option "DontVTSwitch" "true""#));
    }

    #[test]
    fn dummy_xorg_config_depth_24() {
        let cfg = generate_xorg_config(1920, 1080);
        assert!(cfg.contains("DefaultDepth 24"));
    }

    // --- generate_modeline ---

    #[test]
    fn modeline_pixel_clock_scales_with_area() {
        let small = generate_modeline(800, 600, 60);
        let large = generate_modeline(1920, 1080, 60);
        let small_clock: f64 = small.split_whitespace().next().unwrap().parse().unwrap();
        let large_clock: f64 = large.split_whitespace().next().unwrap().parse().unwrap();
        assert!(large_clock > small_clock);
    }

    #[test]
    fn modeline_includes_refresh_dependent_clock() {
        let m30 = generate_modeline(1920, 1080, 30);
        let m60 = generate_modeline(1920, 1080, 60);
        let c30: f64 = m30.split_whitespace().next().unwrap().parse().unwrap();
        let c60: f64 = m60.split_whitespace().next().unwrap().parse().unwrap();
        assert!(c60 > c30, "60Hz should have higher pixel clock than 30Hz");
    }

    #[test]
    fn modeline_blanking_intervals_present() {
        // h_sync_start = width + 48
        let ml = generate_modeline(1920, 1080, 60);
        let parts: Vec<&str> = ml.split_whitespace().collect();
        let h_sync_start: u32 = parts[2].parse().unwrap();
        assert_eq!(h_sync_start, 1920 + 48);
        let h_sync_end: u32 = parts[3].parse().unwrap();
        assert_eq!(h_sync_end, 1920 + 48 + 32);
        let v_sync_start: u32 = parts[6].parse().unwrap();
        assert_eq!(v_sync_start, 1080 + 3);
    }

    #[test]
    fn modeline_4k_dimensions_work() {
        let ml = generate_modeline(3840, 2160, 60);
        let parts: Vec<&str> = ml.split_whitespace().collect();
        assert_eq!(parts[1], "3840");
        assert_eq!(parts[5], "2160");
    }

    // --- which_exists ---

    #[test]
    fn which_exists_finds_common_unix_binary() {
        // /bin/sh exists on every supported platform.
        assert!(which_exists("sh"));
    }

    #[test]
    fn which_exists_returns_false_for_missing_binary() {
        // A 32-character random suffix vastly reduces the odds of a hit.
        assert!(!which_exists("definitely-not-a-real-binary-x9q2r5"));
    }

    // --- is_snap_binary ---

    #[test]
    fn is_snap_binary_false_for_nonexistent() {
        // `which` returns nonzero → path is None → false.
        assert!(!is_snap_binary("definitely-not-a-real-binary-x9q2r5"));
    }

    #[test]
    fn is_snap_binary_false_for_regular_binary() {
        // /bin/sh is not a snap wrapper.
        assert!(!is_snap_binary("sh"));
    }

    // --- find_non_snap_app ---

    #[test]
    fn find_non_snap_app_returns_first_found() {
        // 'sh' is universally available; should be picked first.
        assert_eq!(
            find_non_snap_app(&["definitely-not-a-real-binary-x9q2r5", "sh"]),
            Some("sh")
        );
    }

    #[test]
    fn find_non_snap_app_returns_none_when_all_missing() {
        assert_eq!(
            find_non_snap_app(&[
                "definitely-not-a-real-binary-a",
                "definitely-not-a-real-binary-b"
            ]),
            None
        );
    }

    #[test]
    fn find_non_snap_app_handles_empty_list() {
        assert_eq!(find_non_snap_app(&[]), None);
    }

    // --- is_display_running ---

    #[test]
    fn is_display_running_false_when_no_lockfile() {
        // Display number 99999 has no lock file in /tmp under any
        // realistic test environment.
        assert!(!is_display_running(99_999));
    }

    #[test]
    fn is_display_running_false_for_garbage_lockfile() {
        // Write a lockfile with non-numeric PID — should return false.
        let path = "/tmp/.X88888-lock";
        let _ = std::fs::write(path, "not-a-pid");
        let result = is_display_running(88_888);
        let _ = std::fs::remove_file(path);
        assert!(!result);
    }

    #[test]
    fn is_display_running_false_for_dead_pid() {
        // Write a lockfile with a likely-dead high PID — kill(pid, 0) returns
        // -1 with ESRCH for nonexistent process.
        let path = "/tmp/.X77777-lock";
        // 2^22 - 1 = highest possible PID by default on Linux; the actual
        // process is extremely unlikely to exist mid-test.
        let _ = std::fs::write(path, "4194303\n");
        let result = is_display_running(77_777);
        let _ = std::fs::remove_file(path);
        assert!(!result);
    }

    // --- try_persistent_config_in / ensure_persistent_config ---

    fn temp_home() -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "beam-display-test-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4(),
        ))
    }

    #[test]
    fn try_persistent_config_creates_config_dir_on_first_session() {
        let home = temp_home();
        std::fs::create_dir_all(&home).unwrap();
        let (config_dir, first) = try_persistent_config_in(home.to_str().unwrap()).unwrap();
        assert!(first, "first session should be true");
        assert!(std::path::Path::new(&config_dir).exists());
        // The sentinel file marks initialization complete
        let sentinel = home.join(".local/share/beam/.initialized");
        assert!(sentinel.exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn try_persistent_config_returns_false_for_subsequent_session() {
        let home = temp_home();
        std::fs::create_dir_all(&home).unwrap();
        let (_, first1) = try_persistent_config_in(home.to_str().unwrap()).unwrap();
        assert!(first1);
        // Second call: sentinel exists, so first should be false
        let (_, first2) = try_persistent_config_in(home.to_str().unwrap()).unwrap();
        assert!(!first2, "second session should be false");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn try_persistent_config_seeds_xfce_xml() {
        let home = temp_home();
        std::fs::create_dir_all(&home).unwrap();
        let (config_dir, _) = try_persistent_config_in(home.to_str().unwrap()).unwrap();
        // xfwm4.xml should have been written
        let xfwm = format!("{config_dir}/xfce4/xfconf/xfce-perchannel-xml/xfwm4.xml");
        assert!(std::path::Path::new(&xfwm).exists(), "xfwm4.xml missing");
        let contents = std::fs::read_to_string(&xfwm).unwrap();
        assert!(contents.contains("use_compositing"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn try_persistent_config_seeds_gtk3_settings() {
        let home = temp_home();
        std::fs::create_dir_all(&home).unwrap();
        let (config_dir, _) = try_persistent_config_in(home.to_str().unwrap()).unwrap();
        let gtk = format!("{config_dir}/gtk-3.0/settings.ini");
        assert!(std::path::Path::new(&gtk).exists());
        let contents = std::fs::read_to_string(&gtk).unwrap();
        assert!(contents.contains("gtk-enable-animations=false"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn try_persistent_config_writes_autostart_overrides() {
        let home = temp_home();
        std::fs::create_dir_all(&home).unwrap();
        let (config_dir, _) = try_persistent_config_in(home.to_str().unwrap()).unwrap();
        // Each autostart entry written should be Hidden=true
        let autostart = format!("{config_dir}/autostart");
        assert!(std::path::Path::new(&autostart).exists());
        // Spot-check one entry
        let one = format!("{autostart}/update-notifier.desktop");
        assert!(std::path::Path::new(&one).exists());
        let contents = std::fs::read_to_string(&one).unwrap();
        assert!(contents.contains("Hidden=true"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn try_persistent_config_in_writes_config_version() {
        let home = temp_home();
        std::fs::create_dir_all(&home).unwrap();
        let _ = try_persistent_config_in(home.to_str().unwrap()).unwrap();
        let version = home.join(".local/share/beam/.config-version");
        assert!(version.exists());
        assert_eq!(std::fs::read_to_string(&version).unwrap(), "1");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn try_persistent_config_in_uses_0700_perms() {
        // Config dir must be 0700 (only the user can read).
        use std::os::unix::fs::PermissionsExt;
        let home = temp_home();
        std::fs::create_dir_all(&home).unwrap();
        let (config_dir, _) = try_persistent_config_in(home.to_str().unwrap()).unwrap();
        let mode = std::fs::metadata(&config_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn try_persistent_config_in_fails_when_home_unwritable() {
        // /proc is not writable — try_persistent_config_in should surface an error.
        let result = try_persistent_config_in("/proc/nonexistent-beam-home");
        assert!(result.is_err());
    }

    // (No tests mutating HOME directly — the existing
    // persistent_config_fallback_on_error test already covers the ephemeral
    // fallback path via ensure_persistent_config, and concurrent HOME mutation
    // across tests is unsafe in Rust 2024.)

    // --- seed_default_config ---

    #[test]
    fn seed_default_config_writes_mimeapps_when_browser_detected() {
        // mimeapps.list is only seeded when a non-snap browser is detected.
        // In CI environments that may be absent — guard the assertion.
        let dir = temp_home();
        std::fs::create_dir_all(&dir).unwrap();
        seed_default_config(dir.to_str().unwrap());
        let mimeapps = dir.join("mimeapps.list");
        // Don't assert presence — depends on the test host's installed apps.
        // Instead, assert that IF it exists, it has a known scheme handler.
        if mimeapps.exists() {
            let contents = std::fs::read_to_string(&mimeapps).unwrap();
            assert!(contents.contains("x-scheme-handler/http"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_default_config_writes_helpers_rc() {
        let dir = temp_home();
        std::fs::create_dir_all(&dir).unwrap();
        seed_default_config(dir.to_str().unwrap());
        let helpers = dir.join("xfce4/helpers.rc");
        assert!(helpers.exists());
        let contents = std::fs::read_to_string(&helpers).unwrap();
        assert!(contents.starts_with("[Default]"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_default_config_writes_gtk_css_for_zero_transitions() {
        let dir = temp_home();
        std::fs::create_dir_all(&dir).unwrap();
        seed_default_config(dir.to_str().unwrap());
        let css = dir.join("gtk-3.0/gtk.css");
        assert!(css.exists());
        let contents = std::fs::read_to_string(&css).unwrap();
        assert!(contents.contains("transition-duration: 0s"));
        assert!(contents.contains("animation-duration: 0s"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_default_config_writes_xsettings_no_animations() {
        let dir = temp_home();
        std::fs::create_dir_all(&dir).unwrap();
        seed_default_config(dir.to_str().unwrap());
        let xsettings = dir.join("xfce4/xfconf/xfce-perchannel-xml/xsettings.xml");
        assert!(xsettings.exists());
        let contents = std::fs::read_to_string(&xsettings).unwrap();
        assert!(contents.contains(r#""EnableAnimations" type="bool" value="false""#));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_default_config_writes_keyboard_shortcuts() {
        let dir = temp_home();
        std::fs::create_dir_all(&dir).unwrap();
        seed_default_config(dir.to_str().unwrap());
        let kbd = dir.join("xfce4/xfconf/xfce-perchannel-xml/xfce4-keyboard-shortcuts.xml");
        assert!(kbd.exists());
        let contents = std::fs::read_to_string(&kbd).unwrap();
        // Alt+F2 → app finder is the universal "run app" shortcut.
        assert!(contents.contains("xfce4-appfinder"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_default_config_writes_panel_xml_with_plugins() {
        let dir = temp_home();
        std::fs::create_dir_all(&dir).unwrap();
        seed_default_config(dir.to_str().unwrap());
        let panel = dir.join("xfce4/xfconf/xfce-perchannel-xml/xfce4-panel.xml");
        assert!(panel.exists());
        let contents = std::fs::read_to_string(&panel).unwrap();
        // Required: panel-1 declaration + tasklist + systray + clock plugins.
        assert!(contents.contains("panel-1"));
        assert!(contents.contains("tasklist"));
        assert!(contents.contains("systray"));
        assert!(contents.contains("clock"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_default_config_writes_desktop_wallpaper_xml() {
        let dir = temp_home();
        std::fs::create_dir_all(&dir).unwrap();
        seed_default_config(dir.to_str().unwrap());
        let desktop = dir.join("xfce4/xfconf/xfce-perchannel-xml/xfce4-desktop.xml");
        assert!(desktop.exists());
        let contents = std::fs::read_to_string(&desktop).unwrap();
        assert!(contents.contains("xfce-shapes.svg"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn seed_default_config_writes_session_no_splash() {
        let dir = temp_home();
        std::fs::create_dir_all(&dir).unwrap();
        seed_default_config(dir.to_str().unwrap());
        let session = dir.join("xfce4/xfconf/xfce-perchannel-xml/xfce4-session.xml");
        assert!(session.exists());
        let contents = std::fs::read_to_string(&session).unwrap();
        // splash Engine= "" disables the XFCE splash screen
        assert!(contents.contains(r#""Engine" type="string" value="""#));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- parse_xrandr_connected_output ---

    #[test]
    fn parse_xrandr_extracts_first_connected_output() {
        let output = "\
Screen 0: minimum 1 x 1, current 1920 x 1080, maximum 7680 x 4320
DUMMY0 connected primary 1920x1080+0+0 (normal left inverted right x axis y axis) 0mm x 0mm
   1920x1080     60.00*+
";
        assert_eq!(
            parse_xrandr_connected_output(output),
            Some("DUMMY0".to_string())
        );
    }

    #[test]
    fn parse_xrandr_extracts_dfp_output_for_nvidia() {
        let output = "\
Screen 0: minimum 8 x 8, current 1920 x 1080, maximum 32767 x 32767
DFP-1 connected 1920x1080+0+0 (normal left inverted right x axis y axis) 0mm x 0mm
   1920x1080     60.00 +
";
        assert_eq!(
            parse_xrandr_connected_output(output),
            Some("DFP-1".to_string())
        );
    }

    #[test]
    fn parse_xrandr_returns_none_when_no_connected_output() {
        let output = "\
Screen 0: minimum 1 x 1, current 1920 x 1080, maximum 7680 x 4320
DUMMY0 disconnected (normal left inverted right x axis y axis)
DFP-1 disconnected
";
        assert_eq!(parse_xrandr_connected_output(output), None);
    }

    #[test]
    fn parse_xrandr_returns_none_for_empty_stdout() {
        assert_eq!(parse_xrandr_connected_output(""), None);
    }

    #[test]
    fn parse_xrandr_first_connected_wins_among_many() {
        let output = "\
Screen 0: minimum 1 x 1, current 1920 x 1080, maximum 7680 x 4320
HDMI-A-1 connected primary 1920x1080+0+0 (normal)
DP-1 connected 1280x720+0+0 (normal)
";
        assert_eq!(
            parse_xrandr_connected_output(output),
            Some("HDMI-A-1".to_string())
        );
    }

    #[test]
    fn parse_xrandr_handles_no_space_before_connected() {
        // "disconnected" should NOT match — the " connected" pattern
        // requires a space before to distinguish from "disconnected".
        let output = "\
Screen 0
DP-1 disconnected
DP-2 disconnected (normal)
";
        assert_eq!(parse_xrandr_connected_output(output), None);
    }

    // --- is_benign_xrandr_stderr ---

    #[test]
    fn benign_xrandr_stderr_recognizes_already_exists() {
        assert!(is_benign_xrandr_stderr(
            "xrandr: Mode \"1920x1080\" already exists"
        ));
        assert!(is_benign_xrandr_stderr("already exists"));
    }

    #[test]
    fn benign_xrandr_stderr_rejects_real_errors() {
        assert!(!is_benign_xrandr_stderr(
            "xrandr: Failed to get size of gamma for output"
        ));
        assert!(!is_benign_xrandr_stderr(
            "xrandr: cannot find mode \"3840x2160\""
        ));
        assert!(!is_benign_xrandr_stderr(""));
    }

    #[test]
    fn benign_xrandr_stderr_substring_match() {
        // The check is substring-based, so any line containing the phrase
        // counts as benign even if preceded by other text.
        assert!(is_benign_xrandr_stderr(
            "X Error of failed request: BadMatch\n\
             Major opcode: 153 (XRandR)\n\
             already exists\n"
        ));
    }

    // --- resolve_xorg_invocation ---

    #[test]
    fn xorg_invocation_etc_x11_uses_wrapper_and_relative() {
        // /etc/X11/beam-xorg.conf → Xorg + "beam-xorg.conf" (relative)
        let (bin, arg) = resolve_xorg_invocation("/etc/X11/beam-xorg.conf", true);
        assert_eq!(bin, "Xorg");
        assert_eq!(arg, "beam-xorg.conf");
    }

    #[test]
    fn xorg_invocation_etc_x11_subdir_relative() {
        // Multi-level path under /etc/X11/
        let (bin, arg) = resolve_xorg_invocation("/etc/X11/beam/beam-xorg-20.conf", true);
        assert_eq!(bin, "Xorg");
        assert_eq!(arg, "beam/beam-xorg-20.conf");
    }

    #[test]
    fn xorg_invocation_tmp_uses_direct_binary_when_present() {
        let (bin, arg) = resolve_xorg_invocation("/tmp/beam-xorg-10.conf", true);
        assert_eq!(bin, "/usr/lib/xorg/Xorg");
        assert_eq!(arg, "/tmp/beam-xorg-10.conf");
    }

    #[test]
    fn xorg_invocation_tmp_falls_back_to_xorg_wrapper_when_missing() {
        // When the direct binary doesn't exist, fall back to "Xorg" (PATH lookup).
        let (bin, arg) = resolve_xorg_invocation("/tmp/beam-xorg-10.conf", false);
        assert_eq!(bin, "Xorg");
        assert_eq!(arg, "/tmp/beam-xorg-10.conf");
    }

    #[test]
    fn xorg_invocation_other_path_uses_absolute() {
        // Some custom dev path that isn't /etc/X11 or /tmp — full path passes.
        let (bin, arg) = resolve_xorg_invocation("/home/dev/xorg.conf", true);
        assert_eq!(bin, "/usr/lib/xorg/Xorg");
        assert_eq!(arg, "/home/dev/xorg.conf");
    }

    // --- xorg_config_needs_cleanup ---

    #[test]
    fn xorg_cleanup_true_for_tmp_paths() {
        assert!(xorg_config_needs_cleanup("/tmp/beam-xorg-10.conf"));
        assert!(xorg_config_needs_cleanup("/tmp/anything"));
    }

    #[test]
    fn xorg_cleanup_false_for_etc_paths() {
        // Package configs in /etc/X11/ must not be deleted on drop.
        assert!(!xorg_config_needs_cleanup("/etc/X11/beam-xorg.conf"));
        assert!(!xorg_config_needs_cleanup("/etc/X11/beam/x.conf"));
    }

    #[test]
    fn xorg_cleanup_false_for_other_paths() {
        assert!(!xorg_config_needs_cleanup("/home/dev/xorg.conf"));
        assert!(!xorg_config_needs_cleanup(""));
        assert!(!xorg_config_needs_cleanup("relative/path"));
    }

    #[test]
    fn seed_default_config_writes_all_autostart_overrides() {
        let dir = temp_home();
        std::fs::create_dir_all(&dir).unwrap();
        seed_default_config(dir.to_str().unwrap());
        let autostart_dir = dir.join("autostart");
        assert!(autostart_dir.exists());
        // Spot-check that ALL of the masked-by-design entries exist.
        for entry in [
            "update-notifier.desktop",
            "polkit-gnome-authentication-agent-1.desktop",
            "pulseaudio.desktop",
            "tracker-miner-fs-3.desktop",
            "snap-userd-autostart.desktop",
            "spice-vdagent.desktop",
            "ubuntu-advantage-notification.desktop",
            "ubuntu-report-on-upgrade.desktop",
            "gnome-initial-setup-copy-worker.desktop",
            "gnome-initial-setup-first-login.desktop",
            "org.gnome.DejaDup.Monitor.desktop",
            "org.gnome.Evolution-alarm-notify.desktop",
        ] {
            let path = autostart_dir.join(entry);
            assert!(path.exists(), "Missing autostart override: {entry}");
            let content = std::fs::read_to_string(&path).unwrap();
            assert!(content.contains("Hidden=true"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- pulse_runtime_dir / pulse_config_path / pulse_server_socket_url ---

    #[test]
    fn pulse_runtime_dir_format_locks_to_tmp() {
        assert_eq!(pulse_runtime_dir(10), "/tmp/beam-pulse-10");
        assert_eq!(pulse_runtime_dir(99), "/tmp/beam-pulse-99");
        assert_eq!(pulse_runtime_dir(0), "/tmp/beam-pulse-0");
    }

    #[test]
    fn pulse_runtime_dir_handles_max_display() {
        let result = pulse_runtime_dir(u32::MAX);
        assert!(result.starts_with("/tmp/beam-pulse-"));
        assert!(result.contains(&u32::MAX.to_string()));
    }

    #[test]
    fn pulse_config_path_ends_with_pa() {
        assert_eq!(pulse_config_path(10), "/tmp/beam-pulse-10.pa");
        assert_eq!(pulse_config_path(42), "/tmp/beam-pulse-42.pa");
    }

    #[test]
    fn pulse_server_socket_url_locks_to_unix_scheme() {
        assert_eq!(
            pulse_server_socket_url(10),
            "unix:/tmp/beam-pulse-10/native"
        );
    }

    #[test]
    fn pulse_server_socket_url_is_unix_subpath_of_runtime_dir() {
        // The socket lives at <runtime_dir>/native — verify the helpers
        // stay consistent.
        let runtime = pulse_runtime_dir(7);
        let url = pulse_server_socket_url(7);
        assert!(url.contains(&runtime));
        assert!(url.ends_with("/native"));
    }

    // --- xdg_runtime_dir / keyring_control_dir / xorg paths ---

    #[test]
    fn xdg_runtime_dir_is_tmp_keyed_by_display() {
        assert_eq!(xdg_runtime_dir(10), "/tmp/beam-run-10");
    }

    #[test]
    fn keyring_control_dir_format() {
        assert_eq!(keyring_control_dir(10), "/tmp/beam-keyring-10");
    }

    #[test]
    fn xorg_stderr_log_path_includes_display() {
        assert_eq!(xorg_stderr_log_path(99), "/tmp/beam-xorg-stderr-99.log");
    }

    #[test]
    fn tmp_xorg_config_path_format() {
        assert_eq!(tmp_xorg_config_path(10), "/tmp/beam-xorg-10.conf");
    }

    #[test]
    fn cleanup_paths_are_unique_per_display() {
        // Spot-check that two different display numbers produce different
        // cleanup paths so concurrent agents don't clobber each other.
        assert_ne!(pulse_runtime_dir(10), pulse_runtime_dir(11));
        assert_ne!(xdg_runtime_dir(10), xdg_runtime_dir(11));
        assert_ne!(keyring_control_dir(10), keyring_control_dir(11));
    }

    // --- keyring_data_dir / keyrings_dir (home-relative) ---

    #[test]
    fn keyring_data_dir_under_local_share() {
        let p = keyring_data_dir("/home/alice");
        assert_eq!(p, "/home/alice/.local/share/beam/keyring");
    }

    #[test]
    fn keyrings_dir_is_subdir_of_keyring_data_dir() {
        let p = keyrings_dir("/home/alice");
        assert!(p.starts_with(&keyring_data_dir("/home/alice")));
        assert!(p.ends_with("/keyrings"));
    }

    #[test]
    fn keyring_paths_handle_empty_home() {
        // If HOME is unset, env::var falls back to "" and we still build
        // a (relative) path. Verify it doesn't panic.
        let p = keyring_data_dir("");
        assert!(p.contains(".local/share/beam"));
    }

    // --- pick_panel_plugin ---

    #[test]
    fn panel_plugin_whiskermenu_when_installed() {
        assert_eq!(pick_panel_plugin(true), "whiskermenu");
    }

    #[test]
    fn panel_plugin_applicationsmenu_when_not_installed() {
        assert_eq!(pick_panel_plugin(false), "applicationsmenu");
    }

    // --- browser_to_helper_id ---

    #[test]
    fn helper_id_known_browsers() {
        assert_eq!(browser_to_helper_id("firefox-esr"), "firefox-esr");
        assert_eq!(browser_to_helper_id("firefox"), "firefox");
        assert_eq!(
            browser_to_helper_id("google-chrome-stable"),
            "google-chrome"
        );
        assert_eq!(browser_to_helper_id("google-chrome"), "google-chrome");
        assert_eq!(browser_to_helper_id("chromium-browser"), "chromium");
        assert_eq!(browser_to_helper_id("chromium"), "chromium");
        assert_eq!(browser_to_helper_id("epiphany-browser"), "epiphany");
    }

    #[test]
    fn helper_id_unknown_passes_through() {
        // Unknown browsers fall back to the raw name (defensive — operator
        // may have a non-standard browser configured).
        assert_eq!(browser_to_helper_id("nyxt"), "nyxt");
        assert_eq!(browser_to_helper_id("opera"), "opera");
        assert_eq!(browser_to_helper_id(""), "");
    }

    // --- browser_to_desktop_file ---

    #[test]
    fn desktop_file_known_browsers() {
        assert_eq!(
            browser_to_desktop_file("firefox-esr"),
            "firefox-esr.desktop"
        );
        assert_eq!(browser_to_desktop_file("firefox"), "firefox.desktop");
        assert_eq!(
            browser_to_desktop_file("google-chrome-stable"),
            "google-chrome.desktop"
        );
        assert_eq!(
            browser_to_desktop_file("google-chrome"),
            "google-chrome.desktop"
        );
        assert_eq!(
            browser_to_desktop_file("chromium-browser"),
            "chromium-browser.desktop"
        );
        assert_eq!(
            browser_to_desktop_file("chromium"),
            "chromium-browser.desktop"
        );
        assert_eq!(
            browser_to_desktop_file("epiphany-browser"),
            "org.gnome.Epiphany.desktop"
        );
    }

    #[test]
    fn desktop_file_unknown_returns_empty() {
        // Unknown browsers return empty string — caller skips writing the
        // mimeapps.list file in that case (no MIME associations to wire up).
        assert_eq!(browser_to_desktop_file("nyxt"), "");
        assert_eq!(browser_to_desktop_file(""), "");
    }

    // --- build_helpers_rc ---

    #[test]
    fn helpers_rc_default_only_when_nothing_detected() {
        let s = build_helpers_rc(None, None);
        assert_eq!(s, "[Default]\n");
    }

    #[test]
    fn helpers_rc_includes_terminal_when_detected() {
        let s = build_helpers_rc(Some("xfce4-terminal"), None);
        assert!(s.contains("TerminalEmulator=xfce4-terminal"));
        assert!(!s.contains("WebBrowser="));
    }

    #[test]
    fn helpers_rc_includes_browser_when_detected() {
        let s = build_helpers_rc(None, Some("firefox-esr"));
        assert!(s.contains("WebBrowser=firefox-esr"));
        assert!(!s.contains("TerminalEmulator="));
    }

    #[test]
    fn helpers_rc_includes_both_when_detected() {
        let s = build_helpers_rc(Some("xterm"), Some("chromium"));
        assert!(s.starts_with("[Default]\n"));
        assert!(s.contains("TerminalEmulator=xterm"));
        assert!(s.contains("WebBrowser=chromium"));
    }

    #[test]
    fn helpers_rc_uses_mapped_helper_id() {
        // chromium-browser → chromium (the chromium helper alias).
        let s = build_helpers_rc(None, Some("chromium-browser"));
        assert!(s.contains("WebBrowser=chromium"));
        assert!(!s.contains("WebBrowser=chromium-browser"));
    }

    // --- build_mimeapps_list ---

    #[test]
    fn mimeapps_list_known_browser_returns_some() {
        let r = build_mimeapps_list("firefox-esr").unwrap();
        assert!(r.starts_with("[Default Applications]\n"));
        assert!(r.contains("x-scheme-handler/http=firefox-esr.desktop"));
        assert!(r.contains("x-scheme-handler/https=firefox-esr.desktop"));
        assert!(r.contains("text/html=firefox-esr.desktop"));
        assert!(r.contains("application/xhtml+xml=firefox-esr.desktop"));
    }

    #[test]
    fn mimeapps_list_unknown_browser_returns_none() {
        assert!(build_mimeapps_list("nyxt").is_none());
        assert!(build_mimeapps_list("").is_none());
    }

    #[test]
    fn mimeapps_list_chromium_browser_mapped_correctly() {
        let r = build_mimeapps_list("chromium-browser").unwrap();
        assert!(r.contains("chromium-browser.desktop"));
    }

    #[test]
    fn mimeapps_list_handles_all_known_browsers() {
        for b in [
            "firefox-esr",
            "firefox",
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
            "epiphany-browser",
        ] {
            let r = build_mimeapps_list(b);
            assert!(r.is_some(), "Browser {b} should have a mimeapps entry");
        }
    }

    // --- parse_environ_for_dbus ---

    #[test]
    fn environ_parser_finds_display_and_dbus() {
        let env = b"DISPLAY=:10\0DBUS_SESSION_BUS_ADDRESS=unix:/tmp/dbus\0HOME=/home/x\0";
        let (has_display, dbus) = parse_environ_for_dbus(env, ":10");
        assert!(has_display);
        assert_eq!(dbus.as_deref(), Some("unix:/tmp/dbus"));
    }

    #[test]
    fn environ_parser_no_display_match() {
        let env = b"DISPLAY=:99\0DBUS_SESSION_BUS_ADDRESS=unix:/tmp/x\0";
        let (has_display, dbus) = parse_environ_for_dbus(env, ":10");
        assert!(!has_display);
        // dbus addr is still extracted (the helper extracts both fields).
        assert_eq!(dbus.as_deref(), Some("unix:/tmp/x"));
    }

    #[test]
    fn environ_parser_no_dbus_field() {
        let env = b"DISPLAY=:10\0HOME=/home/x\0";
        let (has_display, dbus) = parse_environ_for_dbus(env, ":10");
        assert!(has_display);
        assert!(dbus.is_none());
    }

    #[test]
    fn environ_parser_empty_input() {
        let (has_display, dbus) = parse_environ_for_dbus(b"", ":10");
        assert!(!has_display);
        assert!(dbus.is_none());
    }

    #[test]
    fn environ_parser_handles_trailing_null() {
        // Real /proc/<pid>/environ ends with a trailing null; verify
        // the split doesn't fail on the empty tail.
        let env = b"DISPLAY=:10\0DBUS_SESSION_BUS_ADDRESS=unix:/tmp/y\0";
        let (has_display, dbus) = parse_environ_for_dbus(env, ":10");
        assert!(has_display);
        assert!(dbus.is_some());
    }

    #[test]
    fn environ_parser_invalid_utf8_does_not_panic() {
        // Some env vars on weird shells contain non-UTF8 bytes. The lossy
        // decode must not panic.
        let env = b"PATH=\xff\xfe/bin\0DISPLAY=:10\0";
        let (has_display, _) = parse_environ_for_dbus(env, ":10");
        assert!(has_display);
    }

    #[test]
    fn environ_parser_matches_only_full_display_token() {
        // DISPLAY=:10 should not match :100 (the display arg).
        let env = b"DISPLAY=:10\0";
        let (has_display, _) = parse_environ_for_dbus(env, ":100");
        assert!(!has_display);
    }

    // --- dbus_address_for_display ---

    #[test]
    fn dbus_addr_for_display_returns_some_on_match() {
        let env = b"DISPLAY=:10\0DBUS_SESSION_BUS_ADDRESS=unix:/tmp/z\0";
        assert_eq!(
            dbus_address_for_display(env, ":10").as_deref(),
            Some("unix:/tmp/z")
        );
    }

    #[test]
    fn dbus_addr_for_display_none_when_display_mismatches() {
        let env = b"DISPLAY=:99\0DBUS_SESSION_BUS_ADDRESS=unix:/tmp/z\0";
        assert!(dbus_address_for_display(env, ":10").is_none());
    }

    #[test]
    fn dbus_addr_for_display_none_when_no_dbus_var() {
        let env = b"DISPLAY=:10\0PATH=/usr/bin\0";
        assert!(dbus_address_for_display(env, ":10").is_none());
    }

    // --- systemd_user_bus_path / systemd_user_bus_address ---

    #[test]
    fn systemd_user_bus_path_includes_uid() {
        assert_eq!(systemd_user_bus_path(1000), "/run/user/1000/bus");
        assert_eq!(systemd_user_bus_path(0), "/run/user/0/bus");
    }

    #[test]
    fn systemd_user_bus_address_unix_path_form() {
        assert_eq!(
            systemd_user_bus_address(1000),
            "unix:path=/run/user/1000/bus"
        );
    }

    #[test]
    fn systemd_user_bus_address_is_subpath_of_bus_path() {
        let path = systemd_user_bus_path(1234);
        let addr = systemd_user_bus_address(1234);
        assert!(addr.contains(&path));
    }
}
