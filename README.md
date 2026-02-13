# Beam

GPU-accelerated remote desktop for Ubuntu, streaming to any browser via WebRTC.

## Features

- **GPU-accelerated encoding** — NVIDIA NVENC, AMD/Intel VA-API, or x264 software fallback
- **Low-latency WebRTC** — sub-30ms on LAN, hardware-decoded in the browser
- **Zero-install client** — works in Chrome, Firefox, Safari. No plugins, no app
- **Multi-user** — isolated virtual desktop sessions with PAM authentication
- **Audio streaming** — PulseAudio capture with Opus encoding
- **Clipboard sync** — copy/paste between local and remote desktops
- **Cursor shape passthrough** — remote cursor shape (text, pointer, resize, etc.) reflected locally
- **Dynamic resolution** — desktop resizes to match your browser viewport
- **Session persistence** — sessions survive server restarts (zero-downtime deploys)
- **Reconnect without re-login** — connection loss shows a reconnect overlay, no need to re-enter credentials
- **Mac keyboard support** — Cmd-to-Ctrl remapping, smooth trackpad scrolling
- **Auto keyboard layout** — detects your keyboard layout and syncs to the remote desktop
- **Adaptive bitrate** — adjusts video quality based on network conditions (VA-API/software encoders)
- **Performance overlay** — press F9 to see RTT, FPS, bitrate, packet loss, and resolution

## Quick Start

### Install from Source

```bash
git clone https://github.com/frecar/beam.git
cd beam
sudo make install         # installs deps, builds, and installs everything
sudo systemctl enable --now beam
```

Open `https://<your-server>:8443` (default port, configurable in `/etc/beam/beam.toml`) and log in with any Linux user account.

### Requirements

- Ubuntu 22.04 or 24.04 LTS (x86_64 or ARM64)
- GPU recommended (NVIDIA, AMD, or Intel) but not required

Rust and Node.js are installed automatically if not present.

### Verify Installation

```bash
make doctor
```

## Configuration

Edit `/etc/beam/beam.toml` (installed) or `config/beam.toml` (development):

```toml
[server]
bind = "0.0.0.0"
port = 8443
# web_root = "/usr/share/beam/web/dist"  # set automatically on install
# tls_cert = "/etc/beam/cert.pem"        # auto-generated if absent
# tls_key = "/etc/beam/key.pem"

[video]
bitrate = 5000      # kbps (initial target)
framerate = 60
# encoder = "nvh264enc"  # auto-detected: nvh264enc > vah264enc > x264enc

[audio]
enabled = true
bitrate = 128       # kbps (Opus)

[session]
max_sessions = 8

[ice]
stun_urls = ["stun:stun.l.google.com:19302", "stun:stun1.l.google.com:19302"]
# turn_urls = ["turn:turn.example.com:3478"]
# turn_username = "beam"
# turn_credential = "secret"
```

## Development

### Setup

```bash
./scripts/dev-setup.sh    # validates toolchain and libraries
make doctor               # shows system status
```

### Build and Run

```bash
make dev                  # builds everything, starts server in debug mode
```

The server runs at `https://localhost:8443` (default port). Log in with your Linux credentials. The `make dev` target ensures `beam-agent` is in PATH so sessions work correctly.

### Common Targets

```
make dev            Build and run server (debug)
make build          Build everything (debug)
make build-release  Build everything (release)
make test           Run Rust tests + TypeScript type check
make lint           Run clippy + tsc
make check          Full pre-commit check (fmt + lint + test)
make doctor         Check system readiness
sudo make deploy    Build release, deploy, restart service
```

### Deploy Changes

After modifying code on a machine running Beam:

```bash
sudo make deploy          # builds release, copies binaries, restarts service
```

## Architecture

```
Browser (TypeScript)         Server (Rust/Axum)           Agent (Rust, per-user)
+-----------------+          +------------------+         +------------------+
| WebRTC receive  |<--SRTP-->| HTTPS + WS       |<-spawn->| XCB/SHM capture  |
| Input capture   |          | PAM auth + JWT   |         | GStreamer encode  |
| Cursor shape    |          | Session persist  |         | WebRTC peer      |
| Clipboard sync  |          | Signaling relay  |         | uinput injection |
| Mac Cmd remap   |          | Token refresh    |         | Clipboard bridge |
| Reconnect UI   |          | Rate limiting    |         | Cursor monitor   |
| Perf overlay   |          +------------------+         +------------------+
+-----------------+                                              |
                                                           Virtual Display
                                                           (Xorg + dummy driver)
                                                           + XFCE4 desktop
                                                           + PulseAudio
```

The server handles authentication and signaling. When a user logs in, it spawns a per-user agent process that creates an isolated virtual display, captures the screen via XCB shared memory, encodes with GStreamer (NVENC/VA-API/x264), and streams to the browser over WebRTC.

## Project Structure

```
beam/
  crates/
    server/     # HTTPS server, auth, session management, signaling
    agent/      # Screen capture, encoding, WebRTC, input injection
    protocol/   # Shared message types and config
  web/          # TypeScript browser client (Vite)
  config/       # Default configuration
  scripts/      # Install/uninstall/dev-setup scripts
  systemd/      # Service file
```

## Keyboard Shortcuts

| Key | Action |
|-----|--------|
| F11 | Toggle fullscreen |
| F9 | Toggle performance overlay (RTT, FPS, bitrate, loss, resolution) |
| Esc | Exit fullscreen |

## Troubleshooting

### Server starts but browser shows blank page
- Check that the web client is built: `ls web/dist/index.html`
- For installed systems, verify `web_root` in `/etc/beam/beam.toml` points to the right directory
- Run `make doctor` to check encoder availability

### Black screen after login
- Check agent logs: `journalctl -u beam -f` and `/tmp/beam-agent-*.log`
- Press F9 to open the performance overlay and check if frames are arriving
- This usually means H.264 frames aren't reaching the browser — force a reconnect (refresh the page)

### Connection fails behind corporate firewall
- WebRTC requires UDP connectivity. Configure a TURN server in `/etc/beam/beam.toml` under `[ice]`

### Non-US keyboard layout
- Beam auto-detects your keyboard layout in Chrome/Edge using the Keyboard Layout Map API
- If auto-detection doesn't work (Firefox, Safari), use the layout selector in the status bar
- Your layout choice is saved automatically for future sessions

## Uninstall

```bash
sudo make uninstall
# or
sudo ./scripts/uninstall.sh
```

## License

MIT
