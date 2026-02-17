# RELEASE PROCESS:
# 1. make bump-version VERSION=x.y.z
# 2. Update CHANGELOG.md (add section for new version)
# 3. make check              (full CI run locally)
# 4. git add -A && git commit -m "release: vx.y.z"
# 5. git tag vx.y.z
# 6. git push && git push --tags
# CI will version-check, build, package, and publish automatically.
#
# PRE-1.0 VERSIONING:
# - Patch (0.1.x): bug fixes, new features, security fixes, improvements
# - Minor (0.x.0): breaking config/protocol changes requiring simultaneous update

.PHONY: build build-release build-web build-rust \
        dev run test lint fmt check ci version-check bump-version \
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
	@echo "  Web client: https://localhost:8444"
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

version-check:
	@CARGO_VER=$$(grep -A5 '^\[workspace\.package\]' Cargo.toml | grep '^version' | sed 's/.*"\(.*\)"/\1/'); \
	WEB_VER=$$(node -p "require('./web/package.json').version"); \
	echo "Cargo.toml version: $$CARGO_VER"; \
	echo "package.json version: $$WEB_VER"; \
	if [ "$$CARGO_VER" != "$$WEB_VER" ]; then \
		echo "ERROR: Version mismatch! Cargo.toml ($$CARGO_VER) != package.json ($$WEB_VER)"; \
		exit 1; \
	fi; \
	if [ -n "$$GITHUB_REF_NAME" ]; then \
		case "$$GITHUB_REF_NAME" in \
			v*) \
				TAG_VER=$${GITHUB_REF_NAME#v}; \
				echo "Git tag version: $$TAG_VER"; \
				if [ "$$CARGO_VER" != "$$TAG_VER" ]; then \
					echo "ERROR: Version mismatch! Cargo.toml ($$CARGO_VER) != git tag ($$TAG_VER)"; \
					exit 1; \
				fi;; \
		esac; \
	fi; \
	echo "Version check passed: $$CARGO_VER"

# Usage: make bump-version VERSION=0.2.0
bump-version:
	@if [ -z "$(VERSION)" ]; then echo "Usage: make bump-version VERSION=x.y.z"; exit 1; fi
	@echo "Bumping version to $(VERSION)..."
	@sed -i '/^\[workspace\.package\]/,/^\[/ s/^version = ".*"/version = "$(VERSION)"/' Cargo.toml
	@node -e "const fs=require('fs'),p=JSON.parse(fs.readFileSync('web/package.json','utf8')); p.version='$(VERSION)'; fs.writeFileSync('web/package.json',JSON.stringify(p,null,2)+'\n')"
	@$(MAKE) version-check
	@echo ""
	@echo "Version bumped to $(VERSION). Next steps:"
	@echo "  1. Update CHANGELOG.md"
	@echo "  2. make check"
	@echo "  3. git add Cargo.toml Cargo.lock web/package.json CHANGELOG.md"
	@echo "  4. git commit -m 'release: v$(VERSION)'"
	@echo "  5. git tag v$(VERSION)"
	@echo "  6. git push && git push --tags"

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
	@scripts/beam-doctor

clean:
	$(CARGO) clean
	rm -rf web/node_modules web/dist
