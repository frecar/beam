#!/usr/bin/env bash
# post-deploy-smoke.sh — runtime smoke test for an ALREADY-RUNNING,
# already-deployed beam-server instance, probed over the network.
#
# This is the deploy-time counterpart to scripts/package-smoke.sh. The two
# cover different gaps and are NOT redundant:
#
#   - package-smoke.sh runs inside the release pipeline. It installs the
#     freshly-built .deb in a throwaway container and boots a fresh server.
#     It proves "this package, in isolation, can boot and serve HTTP".
#
#   - post-deploy-smoke.sh (this script) runs AFTER an upgrade has rolled out
#     to a real host (`apt install beam=X.Y.Z` + service restart). It probes
#     the live, long-lived instance over the network and proves "the upgrade
#     actually took effect on this host and the service is serving traffic".
#     A container that boots fine says nothing about whether the deploy
#     reached the host, whether the service restarted onto the new binary, or
#     whether host-local config/web assets drifted.
#
# What it asserts, in order, against BASE_URL:
#
#   1. GET /api/health eventually returns {"status":"ok"}. Bounded poll with
#      backoff absorbs the brief restart window (old process draining, new
#      process binding the port).
#   2. If EXPECTED_VERSION is set: the version reported by /api/health equals
#      it. This is the single highest-value post-deploy assertion — it is the
#      ONLY one of these checks that a CI package-smoke structurally cannot
#      make. It catches:
#         - a failed restart leaving the OLD process still bound to the port
#           ("apt said OK" but the running binary is stale),
#         - the wrong version pinned / installed,
#         - a half-applied rolling restart.
#      Without EXPECTED_VERSION the smoke still runs but loses most of its
#      post-deploy value, so omitting it emits a loud warning.
#   3. A short settle-then-recheck: after the first healthy response we wait a
#      few seconds and probe /api/health once more. This catches a crash-loop
#      that answers "ok" once and then dies — a single probe would pass it.
#   4. GET / (the SPA root) returns 200 with a stable marker. /api/health can
#      be green while the dashboard is broken if web_root assets drifted on
#      the host (the "/ -> 404 while /health -> 200" failure class), so the
#      health probe alone is not sufficient.
#
# SCOPE (deliberately a live-server HTTP deploy gate, NOT a full session
# validation). It does not open a remote-desktop session, negotiate the
# video pipeline, or exercise GPU/X11/GStreamer — those need real display
# hardware and would make a deploy gate either trivially synthetic or flaky.
#
# Capture readiness is checked SEPARATELY, on the host, not over the network.
# beam-server only relays frames — it never links GStreamer, and capture/encode
# runs in a per-session beam-agent that does not exist on an idle host. So there
# is nothing capture-related for a network probe to honestly assert here. The
# drift that causes a black screen after an upgrade (a missing encoder plugin or
# Xorg dummy driver) is host-local, and `beam-doctor --capture` checks exactly
# that and exits non-zero if the capture stack is broken. A deployment tool
# should run BOTH against each upgraded host: this script over the network, then
#
#   beam-doctor --capture
#
# on the host itself, treating a non-zero exit from either as a failed rollout.
#
# Hermetic and non-flaky: beam-server starts agents lazily per session, so it
# is fully healthy with no display attached. No display/GPU/X11 needed.
#
# Exit non-zero (loudly, with the last response captured) on any failure;
# zero only when every check passes. No `|| true` masking on the assertion
# path — a smoke that cannot fail proves nothing.
#
# Usage:
#   post-deploy-smoke.sh <BASE_URL> [EXPECTED_VERSION]
#   BASE_URL=https://host:8444 EXPECTED_VERSION=1.2.3 post-deploy-smoke.sh
#
# A deployment tool should invoke this against EACH upgraded host directly
# (its own address), not a shared front-end address — see the Load-balancer
# note below.
#
# Environment overrides:
#   BEAM_SMOKE_TIMEOUT    total seconds to wait for the first healthy
#                         response (default 60). A total budget, not a retry
#                         count, so a slow host cannot silently exhaust it.
#   BEAM_SMOKE_SETTLE     seconds to wait before the stability re-check
#                         (default 5).
#   BEAM_SMOKE_MARKER     expected substring in the SPA root HTML
#                         (default "Beam Remote Desktop").
#   BEAM_SMOKE_STRICT_TLS if "1", verify the TLS chain instead of -k. Use for
#                         a real hostname + CA-signed cert; the default
#                         install ships a self-signed cert so -k is the
#                         default. A broken cert-delivery path is invisible
#                         under -k, so strict mode is worth enabling once a
#                         real cert is in place.
#
# Load-balancer / multi-replica note: probing a shared front-end address can
# hit a replica that was NOT just upgraded, so a single probe may report a
# stale version (false RED) or pass on an old node (false GREEN). Always point
# BASE_URL at the specific host you just upgraded.

set -euo pipefail

BASE_URL="${1:-${BASE_URL:-}}"
EXPECTED_VERSION="${2:-${EXPECTED_VERSION:-}}"
TIMEOUT="${BEAM_SMOKE_TIMEOUT:-60}"
SETTLE="${BEAM_SMOKE_SETTLE:-5}"
MARKER="${BEAM_SMOKE_MARKER:-Beam Remote Desktop}"
STRICT_TLS="${BEAM_SMOKE_STRICT_TLS:-0}"

log() { printf '[post-deploy-smoke] %s\n' "$*"; }

if [ -z "${BASE_URL}" ]; then
  log "usage: post-deploy-smoke.sh <BASE_URL> [EXPECTED_VERSION]"
  log "  e.g. post-deploy-smoke.sh https://127.0.0.1:8444 1.2.3"
  exit 2
fi

# Normalise a trailing slash so "${BASE_URL}/api/health" never doubles up.
BASE_URL="${BASE_URL%/}"

command -v curl >/dev/null 2>&1 || { log "curl not found on PATH"; exit 2; }

# Self-signed by default → -k. Strict mode verifies the chain instead.
TLS_FLAG="-k"
if [ "${STRICT_TLS}" = "1" ]; then
  TLS_FLAG=""
  log "strict TLS verification enabled (BEAM_SMOKE_STRICT_TLS=1)"
fi

# Single GET. Echoes "<http_code>\n<body>"; never aborts the script itself
# (callers inspect the code), so connection/TLS failures surface as code 000.
http_get() {
  local url="$1"
  # shellcheck disable=SC2086 # TLS_FLAG is intentionally word-split (may be empty).
  curl ${TLS_FLAG} -sS \
    --connect-timeout 3 --max-time 5 \
    -w '\n%{http_code}' \
    "${url}" 2>/dev/null || printf '\n000'
}

# Extract the "version" field from a /api/health JSON body without requiring
# jq (the smoke must run on a bare host / jump box). Tolerates optional
# whitespace after the colon.
parse_version() {
  printf '%s' "$1" | grep -oE '"version"[[:space:]]*:[[:space:]]*"[^"]*"' \
    | head -1 | sed -E 's/.*"version"[[:space:]]*:[[:space:]]*"([^"]*)".*/\1/'
}

# health_is_ok BODY -> 0 if it contains "status":"ok" (whitespace-tolerant).
health_is_ok() {
  case "$1" in
  *'"status":"ok"'* | *'"status": "ok"'*) return 0 ;;
  *) return 1 ;;
  esac
}

probe_health() {
  local resp code body
  resp="$(http_get "${BASE_URL}/api/health")"
  code="${resp##*$'\n'}"
  body="${resp%$'\n'*}"
  LAST_HEALTH_CODE="${code}"
  LAST_HEALTH_BODY="${body}"
  [ "${code}" = "200" ] && health_is_ok "${body}"
}

# --- preconditions ---------------------------------------------------------
log "Target: ${BASE_URL}"
if [ -n "${EXPECTED_VERSION}" ]; then
  log "Expected version: ${EXPECTED_VERSION}"
else
  log "WARNING: no EXPECTED_VERSION supplied — version match will be SKIPPED."
  log "         The deploy can pass even if the OLD binary is still running."
  log "         Pass the version you just deployed as arg 2 for full coverage."
fi

# --- 1) wait for /api/health -> status=ok ----------------------------------
log "Waiting up to ${TIMEOUT}s for ${BASE_URL}/api/health (status=ok) ..."
LAST_HEALTH_CODE="000"
LAST_HEALTH_BODY=""
deadline=$(($(date +%s) + TIMEOUT))
attempt=0
while :; do
  attempt=$((attempt + 1))
  if probe_health; then
    log "/api/health healthy after ${attempt} attempt(s): ${LAST_HEALTH_BODY}"
    break
  fi
  if [ "$(date +%s)" -ge "${deadline}" ]; then
    log "SMOKE FAILED: /api/health not healthy within ${TIMEOUT}s"
    log "  last HTTP code: ${LAST_HEALTH_CODE}"
    log "  last body:      ${LAST_HEALTH_BODY:-<empty>}"
    exit 1
  fi
  sleep 2
done

# --- 2) version match (the post-deploy-specific assertion) ------------------
if [ -n "${EXPECTED_VERSION}" ]; then
  got_version="$(parse_version "${LAST_HEALTH_BODY}")"
  if [ -z "${got_version}" ]; then
    log "SMOKE FAILED: /api/health did not report a version field"
    log "  body: ${LAST_HEALTH_BODY}"
    exit 1
  fi
  if [ "${got_version}" != "${EXPECTED_VERSION}" ]; then
    log "SMOKE FAILED: version mismatch — the deploy did not take effect"
    log "  expected: ${EXPECTED_VERSION}"
    log "  serving:  ${got_version}"
    log "  (an old process may still be bound, or the wrong version installed)"
    exit 1
  fi
  log "Version OK: serving ${got_version} (matches expected)"
fi

# --- 3) settle + recheck (catch an answer-once-then-crash-loop) -------------
log "Settling ${SETTLE}s then re-probing /api/health for stability ..."
sleep "${SETTLE}"
if ! probe_health; then
  log "SMOKE FAILED: /api/health was healthy then unhealthy ${SETTLE}s later"
  log "  (looks like a crash-loop — the service does not stay up)"
  log "  last HTTP code: ${LAST_HEALTH_CODE}"
  log "  last body:      ${LAST_HEALTH_BODY:-<empty>}"
  exit 1
fi
if [ -n "${EXPECTED_VERSION}" ]; then
  recheck_version="$(parse_version "${LAST_HEALTH_BODY}")"
  if [ "${recheck_version}" != "${EXPECTED_VERSION}" ]; then
    log "SMOKE FAILED: version flipped on re-check (rolling restart in flux?)"
    log "  expected: ${EXPECTED_VERSION}  now serving: ${recheck_version:-<none>}"
    exit 1
  fi
fi
log "Stable: /api/health still healthy after settle window"

# --- 4) SPA root renders ---------------------------------------------------
# A healthy API is not enough: the dashboard is served from web_root, and a
# host-local packaging/config drift can leave the API up while "/" is broken.
log "Checking SPA root ${BASE_URL}/ ..."
root_resp="$(http_get "${BASE_URL}/")"
root_code="${root_resp##*$'\n'}"
root_body="${root_resp%$'\n'*}"
if [ "${root_code}" != "200" ]; then
  log "SMOKE FAILED: SPA root returned HTTP ${root_code} (expected 200)"
  exit 1
fi
case "${root_body}" in
*"${MARKER}"*)
  log "SPA root OK: HTTP 200 with expected marker '${MARKER}'"
  ;;
*)
  log "SMOKE FAILED: SPA root 200 but missing marker '${MARKER}'"
  log "  (web assets may not be served from the expected web_root)"
  exit 1
  ;;
esac

log "POST-DEPLOY SMOKE PASSED"
