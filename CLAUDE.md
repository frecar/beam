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
- Default port: `8444` (avoids conflict with DCV on 8443)
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
3. `Cargo.lock` — regenerate by running `cargo check` after editing Cargo.toml

**Before committing any version bump**: run `make version-check` — this MUST pass or CI will block the release.

### When to Bump

Follow strict semver:
- **Patch** (0.1.1 → 0.1.2): Bug fixes, performance improvements, internal refactors
- **Minor** (0.1.2 → 0.2.0): New features, new capabilities, backward-compatible changes
- **Major** (0.2.0 → 1.0.0): Breaking changes to config, API, or behavior that require user action

### Release Process (DO NOT SKIP STEPS)

1. Update versions in `Cargo.toml` and `web/package.json`, then run `cargo check` to update `Cargo.lock`
2. Validate: `make version-check`
3. Commit all three files: `git add Cargo.toml Cargo.lock web/package.json && git commit -m "Bump version to X.Y.Z"`
4. Tag and push: `git tag vX.Y.Z && git push && git push --tags`
5. CI verifies version match, builds binaries, packages .deb, tests install, publishes to APT repo and GitHub Releases

### Common Mistakes

- Forgetting `web/package.json` → CI rejects the release
- Forgetting `cargo check` after editing Cargo.toml → Cargo.lock out of sync
- Tagging before pushing the version bump commit → tag points to wrong code
- Pushing tag before `git push` on main → CI builds stale code

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
- Split into read-only `is_allowed()` + write `record_failure()` — only failures increment counters
- Dual limiters: username (5 failures / 60s) + IP (20 failures / 60s)
- On success: clear username counter only, NOT the IP counter
- Rationale: one success from IP shouldn't reset brute-force protection against other usernames from the same IP
- Rejected: single combined limiter (too coarse), clearing both on success (creates bypass)

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
- Added: `ProtectKernelTunables`, `ProtectKernelModules`, `ProtectKernelLogs`, `ProtectControlGroups`, `ProtectClock`, `RestrictRealtime`, `LockPersonality`, `TimeoutStopSec=30`
- Note: some broader hardening flags (e.g., `PrivateTmp`, `ProtectSystem=strict`) were relaxed in v0.1.14 due to Xorg/display access requirements — do not blindly re-add them

### udev Rules
- Input device permissions: `MODE="0660" GROUP="input"` (was `0666` world-writable)
- Rationale: input devices should not be readable by arbitrary processes

### Config File Permissions
- `beam.toml` installed as `0640` (was `0644`)
- Rationale: config contains `jwt_secret`; world-readable config is a credential leak
