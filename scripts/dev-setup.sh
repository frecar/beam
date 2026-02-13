#!/usr/bin/env bash
set -euo pipefail

# Beam - Developer environment setup
# Run this once to set up your development environment

log() { echo -e "\033[1;34m[dev-setup]\033[0m $*"; }
err() { echo -e "\033[1;31m[dev-setup]\033[0m $*" >&2; }
ok()  { echo -e "\033[1;32m[dev-setup]\033[0m $*"; }

log "Checking development dependencies..."
ISSUES=0

# Check Rust
if ! command -v rustc &>/dev/null; then
    log "Installing Rust..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
    source "$HOME/.cargo/env"
fi
echo "  Rust: $(rustc --version)"

# Check Node.js
if ! command -v node &>/dev/null; then
    err "Node.js not found. Install it: sudo apt install nodejs npm"
    ISSUES=$((ISSUES + 1))
else
    echo "  Node: $(node --version)"
fi

# Check system libraries
MISSING=()
for lib in gstreamer-1.0 xcb-shm xcb-randr xcb-xfixes libpulse libpulse-simple pam opus; do
    if ! pkg-config --exists "$lib" 2>/dev/null; then
        MISSING+=("$lib")
    fi
done

# Check libclang-dev (required for pam crate bindgen)
if ! dpkg -s libclang-dev >/dev/null 2>&1; then
    MISSING+=("libclang-dev")
fi

if [ ${#MISSING[@]} -gt 0 ]; then
    err "Missing packages: ${MISSING[*]}"
    err "Run: sudo apt install libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev \\"
    err "    libxcb-shm0-dev libxcb-randr0-dev libxcb-xfixes0-dev \\"
    err "    libpulse-dev libpam0g-dev libopus-dev libclang-dev"
    ISSUES=$((ISSUES + 1))
fi

# Check user groups for local testing
log "Checking user groups..."
for group in input video render; do
    if groups | grep -q "\b$group\b"; then
        echo "  $group: ok"
    else
        echo "  $group: MISSING (run: sudo usermod -aG $group $USER)"
        ISSUES=$((ISSUES + 1))
    fi
done

# Install web dependencies
log "Installing web dependencies..."
cd web
npm install
cd ..

# Check GStreamer plugins
log "Checking GStreamer encoders..."
for enc in nvh264enc vah264enc x264enc; do
    if gst-inspect-1.0 "$enc" &>/dev/null; then
        echo "  $enc: available"
    else
        echo "  $enc: not found"
    fi
done

if [ "$ISSUES" -gt 0 ]; then
    err ""
    err "$ISSUES issue(s) found. Fix them and re-run this script."
    exit 1
fi

ok ""
ok "Development environment ready!"
ok ""
ok "  make dev      Build and run (debug mode)"
ok "  make test     Run all tests"
ok "  make check    Full lint + test pass"
ok "  make doctor   Check system readiness"
ok ""
