# Contributing to Beam

## Development Setup

```bash
# Clone the repository
git clone https://github.com/frecar/beam.git
cd beam

# Install development dependencies
./scripts/dev-setup.sh

# Build and run (builds web + Rust, starts server in debug mode)
make dev

# Or build separately:
# cargo build --workspace
# cd web && npm install && npm run build && cd ..
```

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

## Architecture Guidelines

- **Server** handles authentication, session lifecycle, and signaling relay
- **Agent** handles all media: capture, encoding, WebRTC transport, input injection
- **Protocol** contains shared types used by both server and agent
- Each user gets their own agent process running under their UID
- Capture and encoding run in a dedicated thread (not async) for timing precision
- Use channels to bridge between sync capture/encode and async WebRTC/signaling
