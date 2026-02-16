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

- **Version source of truth**: `Cargo.toml` `[workspace.package]` version field
- **Must sync**: `web/package.json` version field (validated by `make version-check`)
- **Semver**: patch for fixes, minor for features, major for breaking changes

### Release Process
1. Bump version in `Cargo.toml` (`[workspace.package]` version) and `web/package.json`
2. Commit: `git commit -am "Bump version to X.Y.Z"`
3. Tag: `git tag vX.Y.Z`
4. Push: `git push && git push --tags`
5. CI builds binaries, packages .deb, tests package, publishes to APT repo and GitHub Releases

### APT Repository
- Hosted on GitHub Pages (`gh-pages` branch)
- URL: `https://frecar.github.io/beam/apt`
- GPG key: `https://frecar.github.io/beam/gpg/beam.gpg`

### Package Paths (must stay consistent across install.sh, Makefile, systemd, nfpm.yaml)
- `/usr/local/bin/beam-server` — signaling server binary
- `/usr/local/bin/beam-agent` — capture agent binary
- `/usr/local/bin/beam-doctor` — diagnostic tool
- `/usr/share/beam/web/dist/` — web client files
- `/etc/beam/beam.toml` — configuration (preserved on upgrade)
- `/etc/systemd/system/beam.service` — systemd unit
- `/etc/udev/rules.d/99-beam-uinput.rules` — uinput permissions
- `/var/lib/beam/sessions/` — runtime session data
