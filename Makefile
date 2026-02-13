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
	@OK=0; WARN=0; FAIL=0; \
	green="\033[32m"; yellow="\033[33m"; red="\033[31m"; bold="\033[1m"; reset="\033[0m"; \
	pass()  { OK=$$((OK+1));   printf "$${green}[OK]$${reset}   %s\n" "$$1"; }; \
	warn()  { WARN=$$((WARN+1)); printf "$${yellow}[WARN]$${reset} %s\n" "$$1"; }; \
	fail()  { FAIL=$$((FAIL+1)); printf "$${red}[FAIL]$${reset} %s\n" "$$1"; }; \
	\
	echo ""; \
	printf "$${bold}Build tools$${reset}\n"; \
	if command -v rustc >/dev/null 2>&1; then \
		ver=$$(rustc --version 2>/dev/null | awk '{print $$2}'); \
		pass "rustc $$ver"; \
	else \
		fail "rustc not found - install via https://rustup.rs"; \
	fi; \
	if command -v cargo >/dev/null 2>&1; then \
		ver=$$(cargo --version 2>/dev/null | awk '{print $$2}'); \
		pass "cargo $$ver"; \
	else \
		fail "cargo not found - install via https://rustup.rs"; \
	fi; \
	if command -v node >/dev/null 2>&1; then \
		ver=$$(node --version 2>/dev/null); \
		pass "node $$ver"; \
	else \
		fail "node not found - install nodejs"; \
	fi; \
	if command -v npm >/dev/null 2>&1; then \
		ver=$$(npm --version 2>/dev/null); \
		pass "npm $$ver"; \
	else \
		fail "npm not found - install nodejs"; \
	fi; \
	if command -v pkg-config >/dev/null 2>&1; then \
		pass "pkg-config found"; \
	else \
		fail "pkg-config not found - install pkg-config"; \
	fi; \
	\
	echo ""; \
	printf "$${bold}Runtime dependencies$${reset}\n"; \
	if command -v gst-inspect-1.0 >/dev/null 2>&1; then \
		ver=$$(gst-inspect-1.0 --version 2>/dev/null | head -1 | awk '{print $$NF}'); \
		major=$$(echo "$$ver" | cut -d. -f1); \
		minor=$$(echo "$$ver" | cut -d. -f2); \
		if [ "$$major" -gt 1 ] 2>/dev/null || { [ "$$major" -eq 1 ] && [ "$$minor" -ge 20 ]; } 2>/dev/null; then \
			if [ "$$minor" -lt 24 ] 2>/dev/null; then \
				warn "gstreamer $$ver (minimum: 1.20, recommended: 1.24+)"; \
			else \
				pass "gstreamer $$ver"; \
			fi; \
		else \
			fail "gstreamer $$ver is too old (minimum: 1.20)"; \
		fi; \
	else \
		fail "gstreamer not found - install gstreamer1.0-tools"; \
	fi; \
	if gst-inspect-1.0 nvh264enc >/dev/null 2>&1; then \
		pass "nvh264enc element available"; \
	else \
		fail "nvh264enc not found - install gstreamer1.0-plugins-bad (NVIDIA GPU required)"; \
	fi; \
	if gst-inspect-1.0 ximagesrc >/dev/null 2>&1; then \
		pass "ximagesrc element available"; \
	else \
		fail "ximagesrc not found - install gstreamer1.0-plugins-good"; \
	fi; \
	if command -v nvidia-smi >/dev/null 2>&1; then \
		if nvidia-smi >/dev/null 2>&1; then \
			gpu=$$(nvidia-smi --query-gpu=name --format=csv,noheader 2>/dev/null | head -1); \
			pass "nvidia gpu: $$gpu"; \
		else \
			fail "nvidia-smi found but failed - check GPU driver"; \
		fi; \
	else \
		fail "nvidia-smi not found - NVIDIA driver not installed"; \
	fi; \
	if command -v Xorg >/dev/null 2>&1; then \
		pass "Xorg found"; \
	else \
		fail "Xorg not found - install xserver-xorg-core"; \
	fi; \
	if command -v xclip >/dev/null 2>&1; then \
		pass "xclip found"; \
	else \
		fail "xclip not found - install xclip"; \
	fi; \
	if command -v xdotool >/dev/null 2>&1; then \
		pass "xdotool found"; \
	else \
		warn "xdotool not found - install xdotool (optional)"; \
	fi; \
	if command -v xrandr >/dev/null 2>&1; then \
		pass "xrandr found"; \
	else \
		fail "xrandr not found - install x11-xserver-utils"; \
	fi; \
	if command -v pulseaudio >/dev/null 2>&1 || command -v pipewire-pulse >/dev/null 2>&1; then \
		if command -v pulseaudio >/dev/null 2>&1; then \
			pass "pulseaudio found"; \
		else \
			pass "pipewire-pulse found"; \
		fi; \
	else \
		fail "pulseaudio/pipewire-pulse not found - install pulseaudio"; \
	fi; \
	if command -v setxkbmap >/dev/null 2>&1; then \
		pass "setxkbmap found"; \
	else \
		fail "setxkbmap not found - install x11-xkb-utils"; \
	fi; \
	\
	echo ""; \
	printf "$${bold}Development libraries$${reset}\n"; \
	if [ -f /usr/include/security/pam_appl.h ] || dpkg -s libpam0g-dev >/dev/null 2>&1; then \
		pass "PAM development headers"; \
	else \
		fail "PAM dev headers not found - install libpam0g-dev"; \
	fi; \
	if pkg-config --exists gstreamer-1.0 2>/dev/null; then \
		pass "gstreamer-1.0 dev (pkg-config)"; \
	else \
		fail "gstreamer-1.0 dev not found - install libgstreamer1.0-dev"; \
	fi; \
	if pkg-config --exists openssl 2>/dev/null; then \
		pass "openssl dev (pkg-config)"; \
	else \
		fail "openssl dev not found - install libssl-dev"; \
	fi; \
	for lib in gstreamer-app-1.0 gstreamer-video-1.0 xcb-shm xcb-randr xcb-xfixes libpulse pam opus; do \
		if pkg-config --exists $$lib 2>/dev/null; then \
			pass "$$lib (pkg-config)"; \
		else \
			fail "$$lib not found (pkg-config)"; \
		fi; \
	done; \
	\
	echo ""; \
	printf "$${bold}System & permissions$${reset}\n"; \
	if [ "$$(id -u)" -eq 0 ]; then \
		warn "running as root - use sudo only for deploy/install"; \
	else \
		pass "running as regular user (uid=$$(id -u))"; \
	fi; \
	if [ -d /var/lib/beam ]; then \
		if [ -w /var/lib/beam ]; then \
			pass "/var/lib/beam exists and is writable"; \
		else \
			warn "/var/lib/beam exists but is not writable by current user"; \
		fi; \
	else \
		warn "/var/lib/beam does not exist (created on deploy)"; \
	fi; \
	if systemctl is-active beam >/dev/null 2>&1; then \
		pass "beam.service is running"; \
	else \
		warn "beam.service is not running"; \
	fi; \
	if [ -f web/dist/index.html ]; then \
		pass "web/dist is built"; \
	else \
		warn "web/dist not built (run make build-web)"; \
	fi; \
	\
	echo ""; \
	total=$$((OK + WARN + FAIL)); \
	printf "$${bold}Doctor summary:$${reset} "; \
	printf "$${green}$$OK passed$${reset}, "; \
	printf "$${yellow}$$WARN warnings$${reset}, "; \
	if [ "$$FAIL" -gt 0 ]; then \
		printf "$${red}$$FAIL failures$${reset}\n"; \
	else \
		printf "$${green}$$FAIL failures$${reset}\n"; \
	fi; \
	echo ""; \
	if [ "$$FAIL" -gt 0 ]; then exit 1; fi

clean:
	$(CARGO) clean
	rm -rf web/node_modules web/dist
