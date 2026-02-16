use anyhow::{Context, Result, bail};
use std::fs;
use std::process::{Child, Command, Stdio};
use tracing::{debug, info, warn};

/// Minimal PulseAudio config for virtual desktop sessions.
/// Creates a null sink (virtual audio output) with a monitor source
/// that the agent can capture from.
const PA_CONFIG: &str = "\
load-module module-null-sink sink_name=beam sink_properties=device.description=Beam
set-default-sink beam
load-module module-native-protocol-unix
load-module module-always-sink
";

/// Manages a virtual X display using the dummy video driver.
pub struct VirtualDisplay {
    display_num: u32,
    xorg_child: Option<Child>,
    desktop_child: Option<Child>,
    pulse_child: Option<Child>,
    cursor_child: Option<Child>,
    /// Temp config path to clean up on drop (None for package-installed static config)
    cleanup_config: Option<String>,
}

impl VirtualDisplay {
    /// Create and start a new virtual X display on the given display number.
    pub fn start(display_num: u32, width: u32, height: u32) -> Result<Self> {
        let config_path = String::from("/etc/X11/beam-xorg.conf");

        // Use the static config installed by the package. When running from
        // source/dev, generate a temporary config in /tmp as fallback.
        if !std::path::Path::new(&config_path).exists() {
            let tmp_config_path = format!("/tmp/beam-xorg-{display_num}.conf");
            let _ = fs::remove_file(&tmp_config_path);
            let config = generate_xorg_config(width, height);
            fs::write(&tmp_config_path, &config)
                .with_context(|| format!("Failed to write Xorg config to {tmp_config_path}"))?;
            return Self::start_with_config(display_num, width, height, tmp_config_path);
        }

        Self::start_with_config(display_num, width, height, config_path)
    }

    fn start_with_config(
        display_num: u32,
        width: u32,
        height: u32,
        config_path: String,
    ) -> Result<Self> {
        let display_str = format!(":{display_num}");

        // Determine how to invoke Xorg based on config location.
        // Package installs: config in /etc/X11/, use Xorg wrapper (setuid) with
        // relative path. Xwrapper.config has allowed_users=anybody +
        // needs_root_rights=yes so Xorg can access /dev/tty0 for VT management.
        // Dev/source installs: config in /tmp, use Xorg binary directly with
        // absolute path (no elevated privilege restrictions).
        let (xorg_bin, config_arg): (&str, &str) = if config_path.starts_with("/etc/X11/") {
            // Relative path required when Xorg runs with elevated privileges
            let filename = config_path.rsplit('/').next().unwrap_or(&config_path);
            // We need to store the filename for the lifetime of the arg
            // Use "Xorg" which resolves to the wrapper
            ("Xorg", filename)
        } else {
            // Dev mode: use direct binary with absolute path
            if std::path::Path::new("/usr/lib/xorg/Xorg").exists() {
                ("/usr/lib/xorg/Xorg", config_path.as_str())
            } else {
                ("Xorg", config_path.as_str())
            }
        };

        // Need to own the config_arg string for the lifetime of the Command
        let config_arg_owned = config_arg.to_string();

        // Capture Xorg stderr to diagnose startup failures
        let xorg_log_path = format!("/tmp/beam-xorg-stderr-{display_num}.log");
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

        // When using the static package config (no per-session modeline),
        // set the requested resolution via xrandr after Xorg starts.
        if config_path == "/etc/X11/beam-xorg.conf"
            && let Err(e) = set_display_resolution(&display_str, width, height)
        {
            warn!("Failed to set initial resolution {width}x{height}: {e}");
        }

        // Only delete temp configs on drop, not the static package config
        let cleanup_config = if config_path.starts_with("/tmp/") {
            Some(config_path)
        } else {
            None
        };

        Ok(Self {
            display_num,
            xorg_child: Some(child),
            desktop_child: None,
            pulse_child: None,
            cursor_child: None,
            cleanup_config,
        })
    }

    /// Change the resolution of the virtual display using xrandr.
    #[allow(dead_code)]
    pub fn set_resolution(&self, width: u32, height: u32) -> Result<()> {
        set_display_resolution(&format!(":{}", self.display_num), width, height)
    }

    /// Start a desktop environment on this display.
    /// Prefers XFCE4 for a full desktop experience. Disables the xfwm4
    /// compositor to minimize latency for remote desktop streaming.
    /// Falls back to openbox (lightweight WM) if XFCE4 is unavailable.
    pub fn start_desktop(&mut self) -> Result<()> {
        let display = format!(":{}", self.display_num);

        // Prefer XFCE4: full desktop with panels, file manager, app menu.
        if which_exists("xfce4-session") {
            // Pre-seed XFCE/GTK config to disable animations, compositor,
            // and menu delays — critical for responsive remote desktop.
            let xfce_config_dir = format!("/tmp/beam-xfce-{}", self.display_num);
            let _ = fs::create_dir_all(&xfce_config_dir);

            let xfconf_dir = format!("{xfce_config_dir}/xfce4/xfconf/xfce-perchannel-xml");
            let _ = fs::create_dir_all(&xfconf_dir);

            // xfwm4: disable compositor and workspace zoom animation
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
  </property>
</channel>
"#,
            );

            // xsettings: disable GTK animations via xsettings daemon
            let _ = fs::write(
                format!("{xfconf_dir}/xsettings.xml"),
                r#"<?xml version="1.0" encoding="UTF-8"?>
<channel name="xsettings" version="1.0">
  <property name="Gtk" type="empty">
    <property name="MenuPopupDelay" type="int" value="0"/>
    <property name="MenuPopdownDelay" type="int" value="0"/>
  </property>
  <property name="Net" type="empty">
    <property name="EnableAnimations" type="bool" value="false"/>
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
            let gtk3_dir = format!("{xfce_config_dir}/gtk-3.0");
            let _ = fs::create_dir_all(&gtk3_dir);
            let _ = fs::write(
                format!("{gtk3_dir}/settings.ini"),
                "[Settings]\n\
                 gtk-enable-animations=false\n\
                 gtk-menu-popup-delay=0\n\
                 gtk-menu-popdown-delay=0\n\
                 gtk-cursor-blink=false\n",
            );

            // GTK3 CSS: kill ALL CSS transitions (Greybird theme has 46 × 200ms
            // transitions on buttons, menus, entries, hover states etc.).
            // gtk-enable-animations only affects GtkAnimation objects, NOT CSS
            // transitions — this override is required for instant menu hover.
            let _ = fs::write(
                format!("{gtk3_dir}/gtk.css"),
                "* { transition-duration: 0s !important; animation-duration: 0s !important; }\n",
            );

            let pulse_server = format!("unix:/tmp/beam-pulse-{}/native", self.display_num);
            let child = Command::new("/usr/bin/dbus-launch")
                .arg("--exit-with-session")
                .arg("xfce4-session")
                .env("DISPLAY", &display)
                .env("PULSE_SERVER", &pulse_server)
                .env("XDG_CONFIG_HOME", &xfce_config_dir)
                // Include GNOME in desktop list so Electron apps (VS Code)
                // auto-detect gnome-libsecret for credential storage.
                // Without this, XFCE falls through to "basic" (weaker encryption).
                .env("XDG_CURRENT_DESKTOP", "XFCE:GNOME")
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .context("Failed to start XFCE4 desktop via dbus-launch")?;

            info!(
                display = self.display_num,
                pid = child.id(),
                "XFCE4 desktop started"
            );

            self.desktop_child = Some(child);

            // Apply settings via xfconf-query AFTER the session starts.
            // Pre-seeded XML files get overridden by xfconfd on startup,
            // so we must set properties after the daemon is running.
            let display_for_xfconf = display.clone();
            std::thread::spawn(move || {
                // Wait for xfconfd and xfce4-panel to initialize
                std::thread::sleep(std::time::Duration::from_secs(3));

                // Discover DBUS_SESSION_BUS_ADDRESS from the running panel.
                // Without this, xfconf-query silently connects to a different
                // (auto-launched) bus instead of the XFCE session's bus,
                // making settings appear to succeed but have no effect.
                let dbus_addr = find_dbus_address_for_display(&display_for_xfconf);
                if dbus_addr.is_none() {
                    warn!("Could not find DBUS session bus, xfconf settings may not apply");
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
                    let keyring_dir = format!("/tmp/beam-keyring-{display_num}");
                    let keyring_data_dir = format!("/tmp/beam-keyring-{display_num}/data");
                    let _ = fs::create_dir_all(&keyring_dir);
                    let _ = fs::create_dir_all(&keyring_data_dir);
                    // Use a shell pipe to reliably deliver the empty password
                    // to --unlock via stdin. Direct Stdio::piped() + drop has
                    // a race condition with --foreground (daemon may not have
                    // started reading stdin when we close the pipe).
                    let keyring_cmd = format!(
                        "echo '' | gnome-keyring-daemon --foreground --unlock \
                         --components=secrets --control-directory={}",
                        keyring_dir
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

                let settings: &[(&str, &str, &str, &str)] = &[
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
                    // Force Greybird theme (consistent, well-tested with our CSS override)
                    ("xsettings", "/Net/ThemeName", "string", "Greybird"),
                    // Replace default Applications Menu with Whisker Menu.
                    // Whisker Menu uses a two-pane layout (categories + apps)
                    // instead of cascading GtkMenu submenus, completely
                    // bypassing the 225ms hardcoded MENU_POPUP_DELAY in GTK3.
                    // Also provides built-in type-to-search.
                    ("xfce4-panel", "/plugins/plugin-1", "string", "whiskermenu"),
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

                // Restart the panel to pick up the Whisker Menu plugin swap.
                // The plugin type change via xfconf only takes effect after
                // the panel reloads its plugin instances.
                let mut cmd = Command::new("xfce4-panel");
                cmd.env("DISPLAY", &display_for_xfconf).arg("--restart");
                if let Some(ref addr) = dbus_addr {
                    cmd.env("DBUS_SESSION_BUS_ADDRESS", addr);
                }
                match cmd.output() {
                    Ok(output) if output.status.success() => {
                        info!("Panel restarted with Whisker Menu");
                    }
                    Ok(output) => {
                        let stderr = String::from_utf8_lossy(&output.stderr);
                        warn!("Panel restart failed: {stderr}");
                    }
                    Err(e) => {
                        warn!("Failed to restart panel: {e}");
                    }
                }

                info!("XFCE settings applied (compositor off, animations off, whisker menu)");
            });

            return Ok(());
        }

        // Fallback: openbox minimal WM
        if which_exists("openbox") {
            let child = Command::new("openbox")
                .env("DISPLAY", &display)
                .env(
                    "PULSE_SERVER",
                    format!("unix:/tmp/beam-pulse-{}/native", self.display_num),
                )
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
        let runtime_dir = format!("/tmp/beam-pulse-{}", self.display_num);
        // Remove stale directory from previous sessions (may be owned by different user)
        let _ = fs::remove_dir_all(&runtime_dir);
        fs::create_dir_all(&runtime_dir)
            .with_context(|| format!("Failed to create PulseAudio dir: {runtime_dir}"))?;

        // Write a minimal PulseAudio config for virtual sessions
        let pa_config_path = format!("/tmp/beam-pulse-{}.pa", self.display_num);
        fs::write(&pa_config_path, PA_CONFIG)
            .with_context(|| format!("Failed to write PA config to {pa_config_path}"))?;

        let child = Command::new("pulseaudio")
            .arg("--daemonize=no")
            .arg("--exit-idle-time=-1")
            .arg("-F")
            .arg(&pa_config_path)
            .env("PULSE_RUNTIME_PATH", &runtime_dir)
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

        // Stop cursor hider
        if let Some(ref mut child) = self.cursor_child {
            stop_child(child, "unclutter", self.display_num);
        }
        // Stop PulseAudio first
        if let Some(ref mut child) = self.pulse_child {
            stop_child(child, "pulseaudio", self.display_num);
        }
        // Stop desktop environment
        if let Some(ref mut child) = self.desktop_child {
            stop_child(child, "desktop", self.display_num);
        }
        // Stop Xorg
        if let Some(ref mut child) = self.xorg_child {
            stop_child(child, "xorg", self.display_num);
        }
        if let Some(ref path) = self.cleanup_config {
            let _ = fs::remove_file(path);
        }
        // Clean up PA and XFCE config files
        let _ = fs::remove_dir_all(format!("/tmp/beam-pulse-{}", self.display_num));
        let _ = fs::remove_file(format!("/tmp/beam-pulse-{}.pa", self.display_num));
        let _ = fs::remove_dir_all(format!("/tmp/beam-xfce-{}", self.display_num));
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
pub fn set_display_resolution(x_display: &str, width: u32, height: u32) -> Result<()> {
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
        if !stderr.contains("already exists") {
            warn!("xrandr --newmode {mode_name} failed: {stderr}");
        }
    }

    // Add mode to the output (may already be added)
    let addmode_output = Command::new("xrandr")
        .env("DISPLAY", x_display)
        .args(["--addmode", "DUMMY0", &mode_name])
        .output()
        .context("Failed to run xrandr --addmode")?;
    if !addmode_output.status.success() {
        let stderr = String::from_utf8_lossy(&addmode_output.stderr);
        if !stderr.contains("already exists") {
            warn!("xrandr --addmode DUMMY0 {mode_name} failed: {stderr}");
        }
    }

    // Switch to the new mode
    let output = Command::new("xrandr")
        .env("DISPLAY", x_display)
        .args(["--output", "DUMMY0", "--mode", &mode_name])
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

/// Discover DBUS_SESSION_BUS_ADDRESS from a running xfce4-panel process on
/// the given display. dbus-launch sets this in child process environments,
/// but doesn't always export it as an X11 root window property. We read it
/// from /proc/<pid>/environ of the panel process.
fn find_dbus_address_for_display(x_display: &str) -> Option<String> {
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
        if has_display {
            if let Some(ref addr) = dbus_addr {
                debug!(
                    x_display,
                    addr, "Found DBUS session address from panel process"
                );
            }
            return dbus_addr;
        }
    }
    warn!(
        x_display,
        "Could not find DBUS_SESSION_BUS_ADDRESS for display"
    );
    None
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
}
