# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
This project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.2.9] - 2026-02-20

### Added
- **Status bar icons**: SVG icons next to all toolbar button labels (Upload, Download, Capture, Mute/Unmute, Fullscreen, Theme, Disconnect, End Session). Dynamic buttons swap icons on state change.

### Fixed
- **Video stall on session reconnection**: Reconnecting to an existing session caused a 20-second timeout because the agent stayed in backgrounded mode (1fps, no keyframes). Server now sends a visibility notification on browser connect, agent forces an IDR keyframe, and the video relay requests a new keyframe if frames are dropped due to broadcast lag.
- **Broadcast channel capacity**: Increased video frame relay buffer from 16 to 64 frames (~530ms at 120fps), preventing keyframe loss during startup burst.

## [0.2.8] - 2026-02-19

### Added
- **Test coverage**: 36 new tests protecting the WebCodecs architecture against regressions
  - 10 signaling channel registry tests (Rust) — channel lifecycle, frame relay, browser kick, text relay
  - 3 boundary-value frame header tests (Rust) — max values, zero values, reserved bytes forward-compat
  - 8 cross-language binary frame parsing tests (TypeScript) — validates Rust/TS header serialization contract
  - 6 WebSocket reconnect logic tests (TypeScript) — exponential backoff, max attempts, auth failure, session replaced
  - 9 WebCodecs renderer state machine tests (TypeScript) — keyframe gating, resolution change, audio mute, cleanup
- **Audio diagnostic logging**: Audio send loop now logs first 3 frames and periodic heartbeat (every 500 frames), matching video's existing logging pattern. Dropped audio frames logged at warn level.
- **Browser audio diagnostics**: Console logs for first audio frame received and first successful decode

### Fixed
- **Deploy creates root-owned files**: `make deploy` no longer depends on `make build-release`, preventing `sudo make deploy` from running the entire build as root. New workflow: `make build-release && sudo make deploy`
- **Stale .js artifacts**: Added `noEmit: true` to `web/tsconfig.json` to prevent TypeScript compiler from emitting .js files alongside .ts sources (Vite handles bundling)

### Changed
- **Documentation accuracy**: Removed stale "adaptive bitrate" feature claim from README (quality selector removed in v0.2.2)
- **Deploy workflow**: README and CLAUDE.md updated to show `make build-release && sudo make deploy`
- **Landing page**: Updated framerate from 60fps to 120fps, removed adaptive bitrate references
- Exported `parseFrameHeader`, `FRAME_HEADER_SIZE`, `FRAME_MAGIC` from `connection.ts` for testability

## [0.2.7] - 2026-02-18

### Fixed
- **Stale WebRTC references**: Removed all remaining WebRTC, STUN/TURN, SRTP, and DataChannel references from documentation, landing page, config templates, and code comments. All text now accurately describes the WebCodecs + WebSocket architecture.
- **Dead stat display code**: Removed `setText` calls for nonexistent ICE/DTLS status panel elements in browser client
- **Stale ICE config template**: Removed `[ice]` section from `beam-production.toml` (config parser has no ICE support since v0.2.0)
- **Landing page architecture diagram**: Fixed "uinput injection" to "XTEST injection" and "WebRTC SRTP" to "WebSocket stream"

## [0.2.6] - 2026-02-18

### Fixed
- **Audio not working on session reuse**: Agent now probes for existing PulseAudio socket when reusing a display, fixing silent audio on reconnect
- **Audio mute preference not persisted**: Browser now restores saved mute preference from localStorage on reconnect instead of always starting muted
- **Auto-unmute on first click**: Only triggers for first-time users who have never set a preference; returning users get their saved preference

### Improved
- **Binary frame relay diagnostics**: Server signaling now logs receiver count and delivery status at trace level for video frame relay troubleshooting

## [0.2.2] - 2026-02-18

### Fixed
- **VideoDecoder keyframe error**: Added keyframe gating after `configureDecoder()` — delta frames before the first keyframe are now skipped instead of causing a `DataError`
- **NVIDIA encoder profile mismatch**: Added capsfilter to force Main profile in NVENC pipeline. Without it, `nvh264enc` could output High profile, mismatching the `avc1.4d0033` codec string and causing `VideoDecoder` decode errors
- **Audio distortion**: Fixed overlapping playback — audio chunks are now sequentially scheduled via `nextAudioPlayTime` tracking instead of all starting at `currentTime`
- **Software encoder OOM**: Capped software encoder (x264enc) to 60fps/20Mbps and bounded appsrc queue to 3 frames to prevent unbounded memory growth
- **Non-passive wheel listener**: Made activity-tracking wheel listener passive to eliminate Chrome console warning

### Removed
- **Quality selector** (High/Low/Auto): Removed broken quality mode toggle — NVIDIA encoders don't support runtime bitrate changes, and the selector caused 0fps on mode switch
- **Dead stats**: Hidden RTT and packet loss from status bar (these were WebRTC stats, no longer available)

### Added
- **Decode time stat**: Status bar now shows `Dec: X.Xms` (time from feed to decoded frame)
- **Default browser**: New sessions auto-configure XDG default browser via `mimeapps.list`, eliminating the "choose your browser" dialog on first click

## [0.2.1] - 2026-02-18

### Fixed
- NVIDIA encoder compatibility: capsfilter for Main profile, audio scheduling, OOM protection for software encoder

## [0.2.0] - 2026-02-17

**BREAKING**: Complete migration from WebRTC to WebCodecs + WebSocket transport.

This is a clean break — there is no backwards compatibility with previous versions. All clients and servers must be updated together.

### Changed
- **Transport**: Replaced WebRTC (SDP, ICE, DTLS, SRTP, RTP, RTCP, DataChannel) with a single TLS WebSocket connection. Video and audio frames are sent as binary WebSocket messages with a 24-byte header. Input events are sent as JSON text messages.
- **Browser decode**: Replaced `<video srcObject>` (WebRTC MediaStream) with WebCodecs `VideoDecoder` + `AudioDecoder` for hardware-accelerated decode and canvas rendering
- **Default framerate**: Bumped from 60 to 120fps
- **Configuration**: Removed `[ice]` section from `beam.toml` (stun_urls, turn_urls, turn_username, turn_credential). No STUN/TURN servers needed.
- **Network**: Only a single HTTPS/WSS port required (default 8444). No ephemeral UDP ports, no ICE negotiation.

### Removed
- All WebRTC dependencies (webrtc-rs, SDP, ICE, DTLS, SRTP, RTP, RTCP)
- DataChannel-based input transport (replaced by WebSocket text messages)
- STUN/TURN configuration and ICE candidate exchange
- Soft reconnect (ICE restart) — replaced by WebSocket reconnection

### Browser Requirements
- Chrome 94+ or Firefox 130+ (WebCodecs API support required)

## [0.1.27] - 2026-02-17

Encoder reliability and application launch fixes.

### Fixed
- **Encoder recreation ignoring config preference**: On resize, reconnection, or pipeline error recovery, the encoder was recreated via auto-detection instead of respecting the configured `encoder` setting. On machines where `nvh264enc` is registered in the GStreamer plugin registry but the GPU is inaccessible, auto-detection would select it, fail to instantiate, and crash the session. Now all encoder recreation paths honor the config preference.
- **Encoder detection false positives**: `detect_encoder()` now uses `ElementFactory::make().build()` (actual instantiation) instead of `ElementFactory::find()` (registry lookup only). This catches cases where a plugin `.so` is installed but the hardware is unavailable (e.g., `nvh264enc` without GPU access).
- **All apps failing with "input/output error"**: `RestrictNamespaces=yes` in the systemd unit installed a seccomp filter that propagated to all child processes, blocking namespace creation. ALL modern browsers (Chrome, Firefox, Epiphany) and snap apps require user namespaces for sandboxing and failed to launch. Removed `RestrictNamespaces` from the service unit.
- **XFCE preferred browser auto-configuration**: XFCE `helpers.rc` now auto-configured to prefer non-snap browser alternatives when available, with a startup warning if none are found. Added `epiphany-browser` (WebKitGTK) to recommended packages as a lightweight non-snap default.

### Removed
- Dead `Encoder::new()` method (all callers now use `with_encoder_preference()`)

## [0.1.26] - 2026-02-17

Clean virtual desktop session startup on Ubuntu 24.04.

### Fixed
- **Error dialog on login**: Removed `:GNOME` from `XDG_CURRENT_DESKTOP` which was activating ~20 GNOME services that crash in a virtual session. Electron/libsecret credential storage works via the D-Bus service name, not `XDG_CURRENT_DESKTOP`
- **gnome-keyring format error**: Stopped pre-creating an empty `login.keyring` file. gnome-keyring uses a binary format; the `--unlock` flag creates it correctly
- **GVFS FUSE mount failure**: Set `GVFS_DISABLE_FUSE=1` to prevent `fusermount3: Permission denied` under systemd restrictions. Thunar file manager still works via GIO API

### Added
- `XDG_RUNTIME_DIR` created per-session for proper D-Bus/GVFS/PulseAudio socket paths
- XDG autostart masking for 12 services that fail in virtual sessions (update-notifier, polkit, tracker-miner, snap-userd, spice-vdagent, etc.)

## [0.1.25] - 2026-02-17

Critical bugfixes: screen capture and agent signaling failures on x86_64 under systemd hardening.

### Fixed
- **MIT-SHM permission denied**: Changed SHM segment permissions from `0o600` to `0o666`. The X server (Xorg) runs as euid=0 via the setuid wrapper, but under systemd's `CapabilityBoundingSet` it lacks `CAP_IPC_OWNER` and cannot bypass IPC permission checks. Without world-accessible permissions, Xorg's `shmat()` fails with `EACCES`. This is safe because `IPC_PRIVATE` segments cannot be discovered by key, and `IPC_RMID` prevents new attachments after both sides connect. This is the standard pattern for X11 MIT-SHM clients.
- **Agent signaling connection failure**: Self-signed TLS cert written with `0600` permissions due to systemd `UMask=0077` overriding the `OpenOptions::mode(0o644)`. Agent (running as non-root user) could not read the cert, fell back to system CA roots, and rejected the self-signed cert as `UnknownIssuer`. Fixed by explicitly calling `set_permissions(0o644)` after file creation, which is not affected by umask.

## [0.1.24] - 2026-02-17

Critical bugfix: screen capture initialization failure on x86_64 systems.

### Fixed
- **MIT-SHM attach failure**: Moved `IPC_RMID` (mark shared memory segment for removal) to after the X server attaches. Previously called before the server's `shmat()`, which Linux blocks on x86_64 kernels. The standard MIT-SHM pattern requires both client and server to be attached before marking for removal.

## [0.1.23] - 2026-02-17

Critical bugfix: agent spawn failure on systems installed via APT package.

### Fixed
- **Agent spawn failure (EPERM)**: `CAP_SYS_NICE` added to systemd `CapabilityBoundingSet`. The kernel refuses to exec binaries with file capabilities (`cap_sys_nice=ep` on beam-agent) when those capabilities are outside the calling process's bounding set. This caused 500 Internal Server Error on every login attempt for APT-installed systems.
- Error logging now shows full error chain (`{e:#}` instead of `{e}`) for session creation, destruction, and agent monitoring errors

## [0.1.22] - 2026-02-17

Code quality, performance, and documentation release: major refactoring of both Rust and TypeScript codebases, capture pipeline optimizations, robustness improvements, and documentation accuracy fixes.

### Changed
- Agent `main.rs` decomposed from 1801 to 1040 lines -- extracted `cli.rs`, `signaling.rs`, `video.rs`, `clipboard_sync.rs`, `file_transfer_task.rs`, `abr.rs` modules
- Web `main.ts` decomposed from 2629 to 1915 lines -- extracted `session.ts`, `ui-state.ts`, `settings.ts`, `login.ts` modules
- Alpha fill loop optimized with u32 OR mask (SIMD-friendly, ~4x fewer iterations at 4K)
- Encoder drain loop: replaced CPU-burning spin_loop() with 100us sleep polling
- File downloads now stream chunks via bounded channel (~350KB peak) instead of collecting all in memory (~135MB peak for 100MB file)
- Desktop process cleanup uses process group kill (setsid + kill -pgid) to properly terminate XFCE grandchild processes
- PAM authentication timeout updated to 30 seconds
- CString construction in session spawn uses proper error propagation instead of unwrap()

### Fixed
- README architecture diagram: corrected "uinput injection" to "XTEST injection" (uinput removed in v0.1.14)
- README and landing page: ABR claims now accurately note NVIDIA uses fixed high-quality CBR
- Capture backpressure: frames skipped when buffer pool exhausted instead of unbounded heap allocation

### Added
- `min_bitrate` and `max_bitrate` documented in README config example (VA-API/software encoders only)
- Screenshot placeholder in README (TODO: add actual screenshot)

## [0.1.21] - 2026-02-17

Security hardening release: rate limiting improvements, file permission tightening, systemd sandboxing, accessibility fixes, and first-boot reliability.

### Security
- Rate limiter: split into read-only check + failure recording; only failed attempts count against the limit
- Rate limiter: dual IP (20/60s) + username (5/60s) limiting to prevent both brute-force and user lockout attacks
- Rate limiter: IPv6 addresses normalized to /64 prefix to prevent per-address rotation bypass
- Rate limiter: separate limiter for release endpoint (no longer shared with login)
- CSP: removed `ws:` from connect-src, only `wss:` allowed
- Admin endpoints: require explicit `admin_users` config (empty = admin panel disabled)
- Admin usernames validated at config load (warns on whitespace, invalid chars)
- Self-signed TLS cert and key persisted to `/var/lib/beam/` with fsync (survives restarts and power loss)
- Self-signed TLS cert auto-regenerated if older than 365 days
- Agent logs moved from `/tmp` to `/var/log/beam/`
- Fixed `constant_time_eq` u8 truncation bug in token comparison (lengths >255 apart would compare as equal)
- udev rules: input device permissions tightened from 0666 to 0660 with GROUP=input
- Config file permissions: 0644 to 0640 (may contain jwt_secret)
- Directory permissions: `/var/lib/beam/` and `/var/log/beam/` set to 0750
- systemd: added CapabilityBoundingSet, ProtectHostname, RestrictNamespaces, RestrictSUIDSGID, UMask=0077, and kernel protection directives

### Added
- SECURITY.md with vulnerability disclosure process, security model, and audit status
- GitHub issue templates: bug reports (with connection type) and feature requests (with workaround field)
- `make bump-version VERSION=x.y.z` for consistent version management
- Version consistency check in CI pipeline
- Startup log when admin panel is disabled (empty `admin_users`)
- Login: rate limit countdown timer with live seconds remaining
- Login: client-side progressive warning after multiple failed attempts
- Login: `Retry-After` header on 429 responses
- Rate limiter: IPv4-mapped IPv6 addresses (::ffff:x.x.x.x) normalized to inner IPv4
- README: GPU prerequisites section, production deployment guide, competitive positioning

### Fixed
- README: corrected port from 8443 to 8444
- README: fixed broken mkcert download URL (was using glob in URL)
- README: updated agent log path from `/tmp` to `/var/log/beam/`
- Landing page: removed stale hardcoded version number, fixed terminal demo accessibility
- Admin delete endpoint: now returns consistent JSON responses (was plain text)
- First-boot crash: `/var/lib/beam/` directory created before TLS cert write
- Self-signed cert persisted across restarts (was regenerated on every restart, breaking session persistence)
- Login button text aligned with subtitle ("Sign in" instead of "Connect")
- Accessibility: 429 errors routed through assertive alert for screen readers
- Accessibility: focus management in loading/error state transitions

### Upgrading from 0.1.20
- Self-signed TLS certificates moved from `/tmp` to `/var/lib/beam/`. New certs are auto-generated on first run
- The CSP now blocks unencrypted WebSocket (`ws:`). This should not affect any deployments since Beam requires TLS

## [0.1.20] - 2026-02-15

### Added
- Admin session management panel
- `--version` and `--help` flags for beam-server and beam-agent
- APT repository with .deb packages for Ubuntu 24.04 (amd64 + arm64)
- `beam-doctor` diagnostic script
- Production config file with documented settings

## [0.1.19] - 2026-02-14

### Added
- Ultra-low latency optimizations (local cursor rendering, CBR low-delay-hq encoder tuning)
- Connection resilience with automatic ICE recovery
- Coalesced mouse moves via requestAnimationFrame
- Unordered DataChannels for input events

## [0.1.18] - 2026-02-13

### Added
- Login page redesign
- Prometheus metrics endpoint (`/metrics`)
- Structured logging with request tracing
- Graceful shutdown with session persistence
- Rate limiting for login attempts
- JWT token auto-refresh
