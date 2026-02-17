# Beam Remote Desktop - Project Context

## Build Commands
- Build everything (debug): `make build`
- Build everything (release): `make build-release`
- Build Rust only: `make build-rust`
- Build Web only: `make build-web`

## Test Commands
- Run all tests: `make test`
- Run Rust tests: `cargo test --workspace`
- Run Web tests: `cd web && npm test`
- Type check Web: `cd web && npx tsc --noEmit`

## Lint and Format
- Run all lints: `make lint`
- Run Rust clippy: `cargo clippy --workspace -- -D warnings`
- Format Rust: `make fmt`
- Full pre-commit check: `make check`
- CI check: `make ci`

## Deployment
- Install to system: `sudo make install`
- Deploy and restart: `sudo make deploy`
- Uninstall: `sudo make uninstall`

## Configuration
- Default port: `8444` (avoids conflict with other services on 8443)
- SPA Fallback: Enabled (unknown paths serve `index.html`)
- Performance:
  - Input: Unordered DataChannels, coalesced mouse moves (RAF)
  - Visual: Local cursor rendering for zero-latency feel
  - Video: Ultra-low latency encoder tuning (`cbr-low-delay-hq`)

## Project Structure
- `crates/agent`: Remote desktop agent (Rust)
- `crates/server`: Signaling and authentication server (Rust)
- `crates/protocol`: Shared message definitions (Rust)
- `web/`: Frontend client (TypeScript/Vite)
- `config/`: Configuration files
- `scripts/`: Setup and installation scripts
- `systemd/`: Systemd service unit

## Versioning & Release

**CRITICAL**: Version bumps require updating THREE files in sync:
1. `Cargo.toml` — `[workspace.package]` version field (source of truth)
2. `web/package.json` — version field (must match exactly)
3. `Cargo.lock` — updated automatically when any `cargo` command runs (e.g., `make check`). No manual step needed if you follow the release workflow

**Before committing any version bump**: run `make version-check` — this MUST pass or CI will block the release.

### When to Bump

Follow strict semver:
- **Patch** (0.1.1 → 0.1.2): Bug fixes, performance improvements, internal refactors
- **Minor** (0.1.2 → 0.2.0): New features, new capabilities, backward-compatible changes
- **Major** (0.2.0 → 1.0.0): Breaking changes to config, API, or behavior that require user action

### Release Process

```bash
# 1. Bump version (updates Cargo.toml, web/package.json, Cargo.lock, package-lock.json)
make bump-version VERSION=X.Y.Z

# 2. Update CHANGELOG.md with the new version section

# 3. Commit everything
git add Cargo.toml Cargo.lock web/package.json web/package-lock.json CHANGELOG.md
git commit -m "release: vX.Y.Z"

# 4. Release (runs full CI, tags, pushes -- all automatic)
make release VERSION=X.Y.Z
```

`make release` validates version sync, runs the full CI suite (fmt, clippy, tests, tsc, vite build), creates the git tag, and pushes both the commit and tag. CI then builds the `.deb` and publishes to the APT repo and GitHub Releases.

**IMPORTANT**: Always use `make release` to tag and push -- never tag manually. This ensures CI passes before a tag is created.

### APT Repository
- Hosted on `gh-pages` branch, served via `raw.githubusercontent.com` (avoids GitHub Pages CDN caching)
- Landing page: `https://frecar.github.io/beam/`
- APT source: `https://raw.githubusercontent.com/frecar/beam/gh-pages`
- GPG key: `https://raw.githubusercontent.com/frecar/beam/gh-pages/gpg/beam.gpg`

### Package Paths (must stay consistent across install.sh, Makefile, systemd, nfpm.yaml)
- `/usr/local/bin/beam-server` — signaling server binary
- `/usr/local/bin/beam-agent` — capture agent binary
- `/usr/local/bin/beam-doctor` — diagnostic tool
- `/usr/share/beam/web/dist/` — web client files
- `/etc/beam/beam.toml` — configuration (preserved on upgrade)
- `/etc/systemd/system/beam.service` — systemd unit
- `/etc/X11/beam-xorg.conf` — static Xorg config for dummy driver
- `/var/lib/beam/sessions/` — runtime session data

## Security Decisions

Recorded: 2026-02-17. These are settled decisions — do not re-debate without a clear reason.

### Rate Limiter Architecture
- Split into read-only `is_allowed()` + write `record_failure()` -- only failures increment counters
- Dual limiters: username (5 failures / 60s) + IP (20 failures / 60s)
- On success: clear username counter only, NOT the IP counter
- Rationale: one success from IP shouldn't reset brute-force protection against other usernames from the same IP
- Rejected: single combined limiter (too coarse), clearing both on success (creates bypass)
- Release endpoint (`/api/sessions/:id/release`) uses a **separate** `release_limiter` (10 failures / 60s per IP) — decoupled from login in v0.1.21. Failed release token guesses no longer affect login availability.
- IPv6 addresses normalized to /64 prefix before rate limiting (`normalize_ip_for_rate_limit`) — prevents per-address rotation bypass from a single /64 allocation. Fixed in v0.1.21.

### Admin Authorization
- Config-based `admin_users` list in `beam.toml`; empty list = admin panel disabled (returns 403)
- No JWT role claims — adds complexity without benefit at current scale
- No Linux group checks — blocking syscalls, breaks in containers
- Rationale: simple, auditable, no syscall risk

### File Paths
- Self-signed TLS cert: `/var/lib/beam/server-cert.pem`
- Agent logs: `/var/log/beam/agent-{id}.log`
- Agent runtime files (PulseAudio socket, Xorg lock, keyring): stay in `/tmp`
- Rationale: agent runs as non-root user; runtime files are ephemeral per-session; `/tmp` is appropriate

### `constant_time_eq` Bug Fix
- Original code: `(a.len() ^ b.len()) as u8` — XOR values >255 apart would truncate to 0 (i.e., compare as equal), breaking timing-safe comparison
- Fixed to: `if a.len() != b.len() { 1u8 } else { 0u8 }` — explicit length mismatch, no truncation
- This was a security bug: attacker could supply a token of sufficiently wrong length and bypass length check

### systemd Hardening
- Full directive set (v0.1.21): `ProtectKernelTunables`, `ProtectKernelModules`, `ProtectKernelLogs`, `ProtectControlGroups`, `ProtectClock`, `ProtectHostname`, `RestrictSUIDSGID`, `LockPersonality`, `UMask=0077`, `TimeoutStopSec=30`
- `RestrictRealtime` is NOT set — beam-agent uses `cap_sys_nice` for real-time frame pacing; seccomp propagates to children, blocking `sched_setscheduler()`
- `RestrictNamespaces` is NOT set (removed v0.1.27) — seccomp propagates to children; ALL modern browsers (Chrome, Firefox, Epiphany) require user namespaces for sandboxing and fail with "input/output error" when blocked
- `CapabilityBoundingSet=CAP_SETUID CAP_SETGID CAP_SETPCAP CAP_AUDIT_WRITE CAP_SYS_NICE` -- minimal set for spawning agent processes as real users. `CAP_SYS_NICE` is required in the bounding set (not effective) because beam-agent has `cap_sys_nice=ep` file capabilities; the kernel refuses to exec binaries with file caps outside the bounding set (EPERM). Fixed in v0.1.23 after production breakage on dev-laptop.
- Note: `PrivateTmp`, `ProtectSystem=strict`, `ProtectHome=yes` were relaxed in v0.1.14 due to Xorg/display access requirements -- do not blindly re-add them
- `RestrictAddressFamilies` is NOT set -- beam-server needs AF_INET, AF_INET6, and AF_UNIX. Adding this is safe but was deferred; add `RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX` when convenient

### udev Rules
- Input device permissions: `MODE="0660" GROUP="input"` (was `0666` world-writable)
- Rationale: input devices should not be readable by arbitrary processes

### Config File Permissions
- `beam.toml` installed as `0640` (was `0644`)
- Rationale: config contains `jwt_secret`; world-readable config is a credential leak

### TLS Certificate Handling
- Self-signed cert persisted to `/var/lib/beam/server-cert.pem` + `server-key.pem` (key file mode 0600)
- On startup: reuse existing cert if files exist and parse successfully; regenerate if missing or corrupt
- Cert age check (v0.1.21): uses file mtime as proxy for expiry — regenerates self-signed cert if >365 days old, warns at startup if >300 days. Does NOT parse x509 `not_after` (avoids adding x509 parsing dependency for self-signed certs). User-provided certs are not age-checked — the user is responsible for rotation.
- Rejected: automatic cert rotation, ACME/Let's Encrypt integration (out of scope for a LAN/home lab tool)

### Admin Error Responses
- Admin endpoints return `"You do not have permission to access this resource"` (generic, no information leakage)
- Rationale (Faramir security review): detailed messages leak config file name, format, and key names to authenticated non-admin users
- Configuration guidance belongs in server startup logs and documentation, not API responses
- Empty `admin_users` list = admin panel disabled (returns 403)
- Startup log emitted when admin panel is disabled

### Frontend Accessibility (Login Flow)
- 429 rate-limit responses: redirect to login form with countdown timer, assertive ARIA alert, submit button disabled during lockout
- Countdown uses `aria-live="assertive"` for first announcement, then `aria-live="polite"` for tick updates (prevents screen reader spam)
- Focus management: login error returns focus to username input; loading state moves focus to cancel button; reconnect overlay focuses reconnect button
- Progressive warning on failed attempts: client-side counter (never server-side — that would be a brute-force oracle per Faramir security review)
- Shake animation on login card for visual feedback on errors
