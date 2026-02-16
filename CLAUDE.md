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
- Hosted on GitHub Pages (`gh-pages` branch)
- Landing page: `https://frecar.github.io/beam/`
- GPG key: `https://frecar.github.io/beam/gpg/beam.gpg`

### Package Paths (must stay consistent across install.sh, Makefile, systemd, nfpm.yaml)
- `/usr/local/bin/beam-server` — signaling server binary
- `/usr/local/bin/beam-agent` — capture agent binary
- `/usr/local/bin/beam-doctor` — diagnostic tool
- `/usr/share/beam/web/dist/` — web client files
- `/etc/beam/beam.toml` — configuration (preserved on upgrade)
- `/etc/systemd/system/beam.service` — systemd unit
- `/etc/X11/beam-xorg.conf` — static Xorg config for dummy driver
- `/var/lib/beam/sessions/` — runtime session data
