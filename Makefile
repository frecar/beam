.PHONY: build build-release build-web build-rust \
        dev run test lint fmt check ci \
        install uninstall deploy clean setup doctor help

CARGO := cargo
NPM := npm
INSTALL_DIR := /usr/local/bin
CONFIG_DIR := /etc/beam
WEB_INSTALL_DIR := /usr/share/beam/web/dist

help:
	@echo "Beam Remote Desktop"
	@echo ""
	@echo "Development:"
	@echo "  make dev            Build everything (debug) and run server"
	@echo "  make build          Build everything (debug)"
	@echo "  make build-release  Build everything (release)"
	@echo "  make test           Run all tests"
	@echo "  make lint           Run clippy + TypeScript type check"
	@echo "  make fmt            Format all Rust code"
	@echo "  make check          Full pre-commit check (fmt + lint + test)"
	@echo "  make ci             Run exact CI checks (verify before pushing)"
	@echo ""
	@echo "Deployment:"
	@echo "  sudo make install   Build and install to system"
	@echo "  sudo make deploy    Build release + restart service"
	@echo "  sudo make uninstall Remove from system"
	@echo ""
	@echo "Setup:"
	@echo "  make setup          Check and install dev dependencies"
	@echo "  make doctor         Check system readiness"

# === Development ===

build: build-web build-rust

build-rust:
	$(CARGO) build --workspace

build-release: build-web
	$(CARGO) build --release --workspace

build-web:
	cd web && $(NPM) install --silent && $(NPM) run build

# Build everything, put agent in PATH, run server
dev: build
	@echo ""
	@echo "Starting Beam server (debug build)..."
	@echo "  Web client: https://localhost:8443"
	@echo "  Log in with your Linux username and password"
	@echo ""
	PATH="$(CURDIR)/target/debug:$$PATH" \
	RUST_LOG=$${RUST_LOG:-info} \
	$(CARGO) run -p beam-server

# Run from release build
run: build-release
	PATH="$(CURDIR)/target/release:$$PATH" \
	RUST_LOG=$${RUST_LOG:-info} \
	./target/release/beam-server

# === Testing ===

test:
	$(CARGO) test --workspace
	cd web && npx tsc --noEmit
	cd web && $(NPM) test

# === Code Quality ===

lint:
	$(CARGO) clippy --workspace -- -D warnings
	cd web && npx tsc --noEmit

fmt:
	$(CARGO) fmt --all

check:
	$(CARGO) fmt --all -- --check
	$(CARGO) clippy --workspace -- -D warnings
	$(CARGO) test --workspace
	cd web && npx tsc --noEmit
	cd web && $(NPM) test
	@echo ""
	@echo "All checks passed."

ci:
	@echo "Running CI checks (mirrors .github/workflows/ci.yml)..."
	$(CARGO) fmt --all -- --check
	$(CARGO) clippy --workspace -- -D warnings
	$(CARGO) test --workspace
	cd web && npx tsc --noEmit && $(NPM) test && $(NPM) run build
	@echo ""
	@echo "All CI checks passed."

# === Installation ===

install:
	@if [ "$$(id -u)" -ne 0 ]; then echo "Run with sudo: sudo make install"; exit 1; fi
	./scripts/install.sh

uninstall:
	@if [ "$$(id -u)" -ne 0 ]; then echo "Run with sudo: sudo make uninstall"; exit 1; fi
	./scripts/uninstall.sh

deploy: build-release
	@if [ "$$(id -u)" -ne 0 ]; then echo "Run with sudo: sudo make deploy"; exit 1; fi
	@echo "Deploying Beam..."
	mkdir -p /var/lib/beam/sessions
	cp target/release/beam-server /tmp/beam-server-new && mv /tmp/beam-server-new $(INSTALL_DIR)/beam-server
	cp target/release/beam-agent /tmp/beam-agent-new && mv /tmp/beam-agent-new $(INSTALL_DIR)/beam-agent
	chmod 755 $(INSTALL_DIR)/beam-server $(INSTALL_DIR)/beam-agent
	rm -rf $(WEB_INSTALL_DIR)/*
	mkdir -p $(WEB_INSTALL_DIR)
	cp -r web/dist/* $(WEB_INSTALL_DIR)/
	setcap cap_sys_nice=ep $(INSTALL_DIR)/beam-agent 2>/dev/null || true
	systemctl restart beam
	@echo "Beam deployed and restarted."

# === Setup ===

setup:
	./scripts/dev-setup.sh

doctor:
	@echo "=== System ==="
	@printf "  %-14s" "Rust:" && (rustc --version 2>/dev/null || echo "NOT FOUND")
	@printf "  %-14s" "Node:" && (node --version 2>/dev/null || echo "NOT FOUND")
	@printf "  %-14s" "GStreamer:" && (gst-inspect-1.0 --version 2>/dev/null | head -1 || echo "NOT FOUND")
	@echo ""
	@echo "=== GPU Encoders ==="
	@for enc in nvh264enc vah264enc x264enc; do \
		printf "  %-14s" "$$enc:"; \
		if gst-inspect-1.0 $$enc >/dev/null 2>&1; then echo "available"; else echo "not found"; fi; \
	done
	@echo ""
	@echo "=== Libraries ==="
	@for lib in gstreamer-1.0 xcb-shm xcb-randr xcb-xfixes libpulse pam opus; do \
		printf "  %-18s" "$$lib:"; \
		if pkg-config --exists $$lib 2>/dev/null; then echo "ok"; else echo "MISSING"; fi; \
	done
	@echo ""
	@echo "=== Service ==="
	@systemctl is-active beam 2>/dev/null && echo "  beam.service: running" || echo "  beam.service: not running"
	@echo ""
	@echo "=== Web Client ==="
	@if [ -f web/dist/index.html ]; then echo "  web/dist: built"; else echo "  web/dist: NOT BUILT (run make build-web)"; fi

clean:
	$(CARGO) clean
	rm -rf web/node_modules web/dist
