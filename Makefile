# RELEASE PROCESS (single command after committing your changes):
#   make release VERSION=x.y.z
#
# This validates versions, runs full CI, tags, and pushes. CI then builds
# the .deb package and publishes to the APT repo automatically.
#
# If you need to bump version separately (e.g., before committing):
#   make bump-version VERSION=x.y.z
#
# PRE-1.0 VERSIONING:
# - Patch (0.1.x): bug fixes, new features, security fixes, improvements
# - Minor (0.x.0): breaking config/protocol changes requiring simultaneous update

.PHONY: build build-release build-web build-rust \
        dev stop run test web-e2e e2e lint fmt fix check ci pre-push install-hooks version-check bump-version \
        coverage coverage-rust coverage-report \
        install uninstall deploy clean setup doctor help

CARGO := cargo
NPM := npm
INSTALL_DIR := /usr/local/bin
CONFIG_DIR := /etc/beam
WEB_INSTALL_DIR := /usr/share/beam/web/dist

# Rust line-coverage floor (#45 ratchet protocol). Single source of truth:
# the CI Coverage job runs `make coverage-rust`, so the gate lives here and
# nowhere else. Ratchet rule: every PR that raises actual coverage bumps this
# to floor(actual - 1); never lower it.
COVERAGE_FLOOR := 87
# Exclude the test-harness modules so the floor tracks real source coverage.
COVERAGE_IGNORE := tests?/
# Per-process .profraw filenames keep test crashes (e.g. GStreamer pipelines
# that can abort) from silently dropping coverage data.
COVERAGE_PROFILE := beam-%p-%m.profraw

help:
	@echo "Beam Remote Desktop"
	@echo ""
	@echo "Development:"
	@echo "  make dev            Build everything (debug) and run server"
	@echo "  make stop           Stop the foreground dev server (no-op; use Ctrl-C)"
	@echo "  make build          Build everything (debug)"
	@echo "  make build-release  Build everything (release)"
	@echo "  make test           Run all tests"
	@echo "  make e2e            Browser E2E smoke suite (Playwright, chromium)"
	@echo "  make coverage       Rust + web coverage (both gated on their floor)"
	@echo "  make coverage-rust  Rust line coverage, enforce ratchet floor (CI gate)"
	@echo "  make coverage-report Rust HTML coverage report (find next gaps)"
	@echo "  make lint           Run clippy + TypeScript type check"
	@echo "  make fmt            Format all Rust code"
	@echo "  make fix            Auto-fix lint + format (Rust + web)"
	@echo "  make check          Full pre-commit check (fmt + lint + test)"
	@echo "  make pre-push       Fast pre-push checks (fmt + type-check, no link)"
	@echo "  make ci             Run exact CI checks (verify before pushing)"
	@echo "  make install-hooks  Install git pre-push hook calling 'make pre-push'"
	@echo ""
	@echo "Deployment:"
	@echo "  sudo make install   Build and install to system"
	@echo "  make build-release && sudo make deploy"
	@echo "                      Build as user, deploy as root"
	@echo "  sudo make uninstall Remove from system"
	@echo ""
	@echo "Release:"
	@echo "  make bump-version VERSION=x.y.z  Bump version in all files"
	@echo "  make release VERSION=x.y.z       Run CI, tag, push (triggers APT build)"
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

# `make dev` runs cargo in the foreground; stop it with Ctrl-C in that terminal.
# `make stop` exists to satisfy the cross-project Makefile contract (help/dev/stop/
# lint/fix/test/ci/build) and is a no-op here because nothing is daemonized.
stop:
	@echo "No background dev services to stop (make dev runs in the foreground; use Ctrl-C)."

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

# === Browser E2E smoke (shared contract) ===

# Build the web bundle, serve it with vite preview, and drive a chromium
# smoke suite against it (Playwright). Installs the chromium browser first
# so a clean checkout works without a separate setup step.
web-e2e:
	cd web && $(NPM) run test:e2e:install
	cd web && $(NPM) run test:e2e

# Standard entry point for the browser smoke suite. Aliases `web-e2e` so the
# `e2e` target name stays consistent with the shared smoke contract even
# though the Playwright command lives under the web tree.
e2e: web-e2e

# === Coverage (#45 ratchet protocol) ===

# Rust line coverage with the ratchet floor enforced. This is the EXACT command
# the CI Coverage job runs — CI calls `make coverage-rust`, so the floor and the
# excludes live only here (no duplicated number to drift, which is what left the
# docs stale before). Fails if line coverage drops below COVERAGE_FLOOR.
coverage-rust:
	LLVM_PROFILE_FILE=$(COVERAGE_PROFILE) $(CARGO) llvm-cov --workspace \
		--ignore-filename-regex '$(COVERAGE_IGNORE)' \
		--fail-under-lines $(COVERAGE_FLOOR)

# Coverage across both stacks: Rust (gated) + web (vitest, gated in vite.config.ts).
coverage: coverage-rust
	cd web && $(NPM) run test:coverage

# Generate a browsable HTML coverage report for finding the next gaps to test.
# Open target/llvm-cov/html/index.html afterwards. Does NOT enforce the floor.
coverage-report:
	LLVM_PROFILE_FILE=$(COVERAGE_PROFILE) $(CARGO) llvm-cov --workspace \
		--ignore-filename-regex '$(COVERAGE_IGNORE)' --html

# === Code Quality ===

lint:
	$(CARGO) clippy --workspace -- -D warnings
	cd web && npx tsc --noEmit
	cd web && $(NPM) run lint
	cd web && $(NPM) run format:check

fmt:
	$(CARGO) fmt --all
	cd web && $(NPM) run format

# Auto-fix lint + format issues across Rust and web. Counterpart to `make lint`/
# `make check`, which only verify. Mirrors the cross-project Makefile contract.
fix:
	$(CARGO) fmt --all
	$(CARGO) clippy --workspace --fix --allow-dirty --allow-staged -- -D warnings
	cd web && $(NPM) run lint:fix
	cd web && $(NPM) run format

check:
	$(CARGO) fmt --all -- --check
	$(CARGO) clippy --workspace -- -D warnings
	$(CARGO) test --workspace
	cd web && npx tsc --noEmit
	cd web && $(NPM) run lint
	cd web && $(NPM) run format:check
	cd web && $(NPM) run test:coverage
	cd web && $(NPM) run build
	cd web && $(NPM) run check:bundle
	cd web && $(NPM) run audit
	@echo ""
	@echo "All checks passed."

ci:
	@echo "Running CI checks (mirrors .github/workflows/ci.yml)..."
	$(CARGO) fmt --all -- --check
	$(CARGO) clippy --workspace -- -D warnings
	$(CARGO) test --workspace
	cd web && npx tsc --noEmit
	cd web && $(NPM) run lint
	cd web && $(NPM) run format:check
	cd web && $(NPM) run test:coverage
	cd web && $(NPM) run build
	cd web && $(NPM) run check:bundle
	cd web && $(NPM) run audit
	@echo ""
	@echo "All CI checks passed."

# Fast pre-push: fmt + type-check only. No clippy/test/web-build because
# those need libclang+pkg-config+gstreamer-dev installed (linker step).
# GitHub Actions runs the full `make ci` on every PR — that's the
# canonical correctness gate. This target keeps the dev loop quick and
# unblocks contributors who haven't installed system dev headers.
pre-push:
	@echo "Running fast pre-push checks (fmt + type-check only)..."
	$(CARGO) fmt --all -- --check
	cd web && npx tsc --noEmit
	@echo ""
	@echo "Pre-push checks passed. CI will run the full clippy+test+build suite."

install-hooks:
	@bash scripts/install-hooks.sh

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
	@cd web && npm install --package-lock-only --silent 2>/dev/null || true
	@$(CARGO) check --quiet
	@$(MAKE) version-check

# Full release: bump, check, commit, tag, push. Requires VERSION and CHANGELOG already updated.
# Usage: make release VERSION=0.2.0
release:
	@if [ -z "$(VERSION)" ]; then echo "Usage: make release VERSION=x.y.z"; exit 1; fi
	@if [ -n "$$(git status --porcelain)" ]; then echo "ERROR: Working tree not clean. Commit or stash changes first."; exit 1; fi
	@CURRENT_VER=$$(grep -A5 '^\[workspace\.package\]' Cargo.toml | grep '^version' | sed 's/.*"\(.*\)"/\1/'); \
	if [ "$$CURRENT_VER" != "$(VERSION)" ]; then \
		echo "ERROR: Cargo.toml version ($$CURRENT_VER) != requested version ($(VERSION))"; \
		echo "Run 'make bump-version VERSION=$(VERSION)' first, update CHANGELOG.md, then commit."; \
		exit 1; \
	fi
	@$(MAKE) version-check
	@$(MAKE) ci
	@echo ""
	@echo "All checks passed. Tagging v$(VERSION) and pushing..."
	git tag v$(VERSION)
	git push && git push --tags
	@echo ""
	@echo "Release v$(VERSION) pushed. CI will build .deb and publish to APT repo."

# === Installation ===

install:
	@if [ "$$(id -u)" -ne 0 ]; then echo "Run with sudo: sudo make install"; exit 1; fi
	./scripts/install.sh

uninstall:
	@if [ "$$(id -u)" -ne 0 ]; then echo "Run with sudo: sudo make uninstall"; exit 1; fi
	./scripts/uninstall.sh

deploy:
	@if [ "$$(id -u)" -ne 0 ]; then echo "Run with sudo: sudo make deploy"; exit 1; fi
	@if [ ! -f target/release/beam-server ] || [ ! -f target/release/beam-agent ]; then \
		echo "ERROR: Release binaries not found. Run 'make build-release' first."; exit 1; fi
	@if [ ! -d web/dist ]; then \
		echo "ERROR: web/dist not found. Run 'make build-release' first."; exit 1; fi
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
