# Contributing to Beam

## Development Setup

```bash
# Clone the repository
git clone https://github.com/frecar/beam.git
cd beam

# Install development dependencies
./scripts/dev-setup.sh

# Install the git pre-push hook (fast checks only)
make install-hooks

# Build and run (builds web + Rust, starts server in debug mode)
make dev

# Or build separately:
# cargo build --workspace
# cd web && npm install && npm run build && cd ..
```

## Git hooks

`make install-hooks` installs a pre-push hook that runs `make pre-push` —
`cargo fmt --check` plus `tsc --noEmit`. These are *cheap* checks that don't
need libclang/pkg-config/gstreamer dev headers, so docs-only PRs (and
contributors who haven't installed system dev deps) are not blocked.

GitHub Actions runs the full `make ci` (clippy + test + web build) on every
PR — that's the canonical correctness gate. The local pre-push hook exists
to catch obvious style/type slips before they bounce off CI, not to mirror CI.

## Code Style

### Rust
- Run `cargo fmt` before committing
- Run `cargo clippy --workspace` and fix all warnings
- Use `anyhow::Result` in binary crates, `thiserror` in libraries
- Use `tracing` for logging, never `println!`
- Document all public APIs with `///` doc comments

### TypeScript
- Strict mode enabled
- No `any` types
- Use modern ES2022+ features

## Testing

```bash
# Run all tests
cargo test --workspace

# Run a specific crate's tests
cargo test -p beam-agent

# Web client type checking
cd web && npx tsc --noEmit
```

## Versioning & Release

Beam follows [semver](https://semver.org/). Three files must stay in sync on
every version bump:

1. `Cargo.toml` — `[workspace.package]` version (source of truth)
2. `web/package.json` — must match exactly
3. `Cargo.lock` — updated automatically by `cargo` commands

Use the helpers:

```bash
make bump-version VERSION=X.Y.Z   # updates all three files in sync
make version-check                # asserts they match (also runs in CI)
make release VERSION=X.Y.Z        # full CI + tag + push (triggers .deb build via Actions)
```

Never tag manually — always `make release`. The release workflow refuses to
tag if `make version-check` fails or any CI step doesn't pass.

For the full release pipeline (CI → APT publish → deploy), see `CLAUDE.md`.

## Commit message conventions

Conventional commits:

- `feat:` — new feature
- `fix:` — bug fix
- `refactor:` — code change that neither adds a feature nor fixes a bug
- `chore:` — tooling, dev-experience, hygiene
- `docs:` — documentation only
- `test:` — adding/updating tests
- `ci:` — GitHub Actions / build pipeline
- `perf:` — performance improvement
- `style:` — formatting, whitespace
- `build:` — build system / dependency changes
- `revert:` — revert a prior commit
- `deps:` — dependency bumps
- `release:` — reserved for the version-bump commit produced by `make bump-version`

The pre-commit hook in `.pre-commit-config.yaml` rejects commits that
don't match one of these prefixes.

## Architecture Guidelines

- **Server** handles authentication, session lifecycle, and signaling relay
- **Agent** handles all media: capture, encoding, WebSocket streaming, input injection
- **Protocol** contains shared types used by both server and agent
- Each user gets their own agent process running under their UID
- Capture and encoding run in a dedicated thread (not async) for timing precision
- Use channels to bridge between sync capture/encode and async WebSocket/signaling
