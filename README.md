# Beam

GPU-accelerated remote desktop for Ubuntu, streaming to any browser via WebRTC.

<!-- Replace with actual screenshot: ![Beam desktop session](docs/screenshot.png) -->

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

## Install (Ubuntu 24.04)

### APT Repository (Recommended)

```bash
curl -fsSL https://frecar.github.io/beam/gpg/beam.gpg | sudo tee /etc/apt/keyrings/beam.gpg > /dev/null
echo "deb [arch=$(dpkg --print-architecture) signed-by=/etc/apt/keyrings/beam.gpg] https://frecar.github.io/beam/apt stable main" | sudo tee /etc/apt/sources.list.d/beam.list
sudo apt-get update && sudo apt-get install beam
```

Or use the one-liner:
```bash
curl -fsSL https://frecar.github.io/beam/install | sudo bash
```

After installation, open `https://<your-server>:8444` and log in with any Linux user account.

### Build from Source

```bash
git clone https://github.com/frecar/beam.git
cd beam
sudo make install
sudo systemctl enable --now beam
```

Requires Ubuntu 24.04 (x86_64 or ARM64). GPU recommended but not required. Rust and Node.js are installed automatically if not present.

## Verify

```bash
beam-server --version
beam-doctor
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
# max_width = 3840       # clamp resolution (default: 3840, 0 = unlimited)
# max_height = 2160      # clamp resolution (default: 2160, 0 = unlimited)

[audio]
enabled = true
bitrate = 128       # kbps (Opus)

[session]
max_sessions = 8
# idle_timeout = 3600  # seconds (0 = disabled, default: 3600)

[ice]
stun_urls = ["stun:stun.l.google.com:19302", "stun:stun1.l.google.com:19302"]
# turn_urls = ["turn:turn.example.com:3478"]
# turn_username = "beam"
# turn_credential = "secret"
```

### TLS Certificate

Beam auto-generates a self-signed certificate on first run. Browsers will show a security warning — click through it or set up a trusted certificate:

**Option A: mkcert (recommended for LAN/dev)**
```bash
# Install mkcert (creates a local CA trusted by your browser)
sudo apt install libnss3-tools
curl -JLO "https://github.com/FiloSottile/mkcert/releases/latest/download/mkcert-v*-linux-amd64"
sudo mv mkcert-* /usr/local/bin/mkcert && sudo chmod +x /usr/local/bin/mkcert
mkcert -install

# Generate cert for your hostname
mkcert -cert-file /etc/beam/cert.pem -key-file /etc/beam/key.pem "$(hostname)" "$(hostname -I | awk '{print $1}')"
sudo systemctl restart beam
```

**Option B: Let's Encrypt (internet-facing servers)**
```bash
sudo apt install certbot
sudo certbot certonly --standalone -d beam.example.com
# Update /etc/beam/beam.toml:
# tls_cert = "/etc/letsencrypt/live/beam.example.com/fullchain.pem"
# tls_key = "/etc/letsencrypt/live/beam.example.com/privkey.pem"
sudo systemctl restart beam
```

**Option C: Existing certificate** — set `tls_cert` and `tls_key` in `/etc/beam/beam.toml`.

### Keyboard Shortcuts

| Key | Action |
|-----|--------|
| F11 | Toggle fullscreen |
| F9 | Toggle performance overlay (RTT, FPS, bitrate, loss, resolution) |
| Esc | Exit fullscreen |

## Troubleshooting

Run the diagnostic tool:
```bash
beam-doctor
```

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

View logs:
```bash
journalctl -u beam -f
```

## Uninstall

**APT package:**
```bash
sudo apt-get remove beam        # keep configuration
sudo apt-get purge beam         # remove everything
```

**Source install:**
```bash
sudo make uninstall
```

---

## Contributing

### Development Setup

```bash
./scripts/dev-setup.sh
make doctor
```

### Build and Run

```bash
make dev                  # builds everything, starts server in debug mode
```

The server runs at `https://localhost:8444`. Log in with your Linux credentials.

### Make Targets

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

### Architecture

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

### Project Structure

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

## License

MIT
