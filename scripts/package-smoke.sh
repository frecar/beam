#!/usr/bin/env bash
# package-smoke.sh — runtime smoke test for an installed beam .deb.
#
# Verifies that the packaged beam-server actually BOOTS and serves its
# HTTP surface — not merely that the binary exists and prints --version.
# This closes the "build-green != runtime-works" gap: a .deb that installs
# cleanly but crashes on startup, or serves the SPA root as 500/404 while
# binding the port, would otherwise ship undetected.
#
# Intended to run inside the release pipeline's package-install container
# (a clean ubuntu:24.04 with the .deb already installed). It only exercises
# beam-server's HTTP endpoints — it never starts beam-agent, which needs a
# real X11 display + GStreamer + PulseAudio (out of scope for a package
# smoke; see the Playwright / xvfb follow-ups). beam-server itself spawns
# agents lazily per session, so it boots fine with no display.
#
# Exit non-zero (loudly, with server logs) on any failure.

set -euo pipefail

PORT="${BEAM_SMOKE_PORT:-8444}"
HOST="127.0.0.1" # IPv4 explicitly: the server binds 0.0.0.0, so avoid any
# IPv6 'localhost' resolution wrinkle in minimal containers.
BASE="https://${HOST}:${PORT}"
CONFIG="${BEAM_SMOKE_CONFIG:-/etc/beam/beam.toml}"
LOG="$(mktemp /tmp/beam-server-smoke.XXXXXX.log)"
READY_TIMEOUT="${BEAM_SMOKE_TIMEOUT:-30}" # seconds to wait for /api/health
SERVER_PID=""

log() { printf '[package-smoke] %s\n' "$*"; }

dump_server_log() {
  log "----- beam-server log (${LOG}) -----"
  cat "${LOG}" 2>/dev/null || log "(no log captured)"
  log "------------------------------------"
}

cleanup() {
  if [ -n "${SERVER_PID}" ] && kill -0 "${SERVER_PID}" 2>/dev/null; then
    kill "${SERVER_PID}" 2>/dev/null || true
    wait "${SERVER_PID}" 2>/dev/null || true
  fi
  rm -f "${LOG}"
}
trap cleanup EXIT

fail() {
  log "SMOKE FAILED: $*"
  dump_server_log
  exit 1
}

# --- preconditions ---------------------------------------------------------
command -v beam-server >/dev/null 2>&1 || fail "beam-server not on PATH"
command -v curl >/dev/null 2>&1 || fail "curl not installed in smoke container"
[ -f "${CONFIG}" ] || fail "config not found at ${CONFIG} (.deb did not install it?)"

# --- start the server ------------------------------------------------------
log "Starting beam-server (--config ${CONFIG}, port ${PORT})..."
beam-server --config "${CONFIG}" --port "${PORT}" >"${LOG}" 2>&1 &
SERVER_PID=$!

# --- wait for /api/health to return status=ok ------------------------------
# Self-signed TLS (-k). The cert is generated BEFORE the listener binds, so
# the only race is "not listening yet" — a bounded poll covers it. We also
# check the process is still alive each iteration so a startup crash fails
# fast (with logs) instead of waiting out the full timeout.
log "Waiting up to ${READY_TIMEOUT}s for ${BASE}/api/health ..."
deadline=$(( $(date +%s) + READY_TIMEOUT ))
health_body=""
while :; do
  if ! kill -0 "${SERVER_PID}" 2>/dev/null; then
    fail "beam-server exited during startup (before becoming healthy)"
  fi
  if health_body=$(curl -k -fsS --max-time 2 "${BASE}/api/health" 2>/dev/null); then
    break
  fi
  if [ "$(date +%s)" -ge "${deadline}" ]; then
    fail "timed out after ${READY_TIMEOUT}s waiting for /api/health"
  fi
  sleep 1
done

case "${health_body}" in
*'"status":"ok"'* | *'"status": "ok"'*)
  log "/api/health OK: ${health_body}"
  ;;
*)
  fail "/api/health returned unexpected body: ${health_body}"
  ;;
esac

# --- assert the SPA root renders (the '/ -> 404 while /health -> 200' class)
# A healthy /api/health is NOT enough: the dashboard is served from web_root,
# and a packaging/config mismatch can leave the binary up while '/' is broken.
log "Checking SPA root ${BASE}/ ..."
root_status=$(curl -k -sS -o /tmp/beam-smoke-root.html -w '%{http_code}' --max-time 5 "${BASE}/" || echo "000")
[ "${root_status}" = "200" ] || fail "SPA root returned HTTP ${root_status} (expected 200)"
grep -q "Beam Remote Desktop" /tmp/beam-smoke-root.html \
  || fail "SPA root 200 but missing expected marker 'Beam Remote Desktop' (web assets not served from web_root?)"
log "SPA root OK: HTTP 200 with expected marker"

rm -f /tmp/beam-smoke-root.html
log "PACKAGE SMOKE PASSED"
