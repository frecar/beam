# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
This project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
