# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).
This project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.1.21] - 2026-02-17

### Security
- Rate limiter: split into read-only check + failure recording; only failed attempts count against the limit
- Rate limiter: dual IP (20/60s) + username (5/60s) limiting to prevent both brute-force and user lockout attacks
- Admin endpoints: require explicit `admin_users` config (empty = admin panel disabled)
- Release endpoint: rate-limited per IP to prevent token brute-force
- Self-signed TLS cert and key persisted to `/var/lib/beam/` (survives restarts, no longer in world-readable `/tmp`)
- Agent logs moved from `/tmp` to `/var/log/beam/`
- Fixed `constant_time_eq` u8 truncation bug in token comparison (lengths >255 apart would compare as equal)
- udev rules: input device permissions tightened from 0666 to 0660 with GROUP=input
- Config file permissions: 0644 to 0640 (may contain jwt_secret)
- Directory permissions: `/var/lib/beam/` and `/var/log/beam/` set to 0750
- systemd: added CapabilityBoundingSet and kernel protection directives
- Admin error responses no longer leak config file details

### Added
- SECURITY.md with vulnerability disclosure process and security model documentation
- GitHub issue templates for bug reports and feature requests
- `make bump-version VERSION=x.y.z` for consistent version management
- Version consistency check in CI pipeline
- Startup log when admin panel is disabled (empty `admin_users`)

### Fixed
- README: corrected port from 8443 to 8444
- README: updated agent log path from `/tmp` to `/var/log/beam/`
- Landing page: removed stale hardcoded version number
- Admin delete endpoint: now returns consistent JSON responses (was plain text)
- First-boot crash: `/var/lib/beam/` directory created before TLS cert write
- Self-signed cert persisted across restarts (was regenerated on every restart, breaking session persistence)

## [0.1.20] - 2026-02-15

### Added
- Admin session management panel
- `--version` and `--help` flags for beam-server and beam-agent
- APT repository with .deb packages for Ubuntu 24.04 (amd64 + arm64)
- `beam-doctor` diagnostic script
- Production config file with documented settings

### Changed
- Default port changed to 8444 (avoids conflict with DCV on 8443)
- SPA fallback enabled for client-side routing

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
