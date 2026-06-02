#!/usr/bin/env bash
# agent-display-smoke.sh — runtime smoke test for beam-agent against a
# simulated X display.
#
# Verifies that beam-agent actually BOOTS its capture + encode pipeline
# against a real (virtual) X server and a real GStreamer software encoder —
# not merely that the binary builds and unit-tests pass with trait-injected
# mocks. This closes the "mocks-green != runtime-works" gap for the classes
# of breakage that mocks structurally cannot catch:
#
#   - GStreamer plugin availability at runtime (x264enc / h264parse /
#     videoconvert / appsrc / appsink must be loadable, not just linkable).
#   - X server connection + MIT-SHM screen capture against a live display.
#   - xclip presence (ClipboardBridge::new aborts the agent if it is missing,
#     before the capture thread ever starts).
#
# Approach (the "existing display" path): we pre-start a virtual display with
# Xvfb, so beam-agent connects to it instead of trying to spawn its own real
# Xorg + xf86-video-dummy server (which needs setuid/root and is awkward in
# CI). The agent's capture+encode thread runs independently of the server
# WebSocket connection, so we point --server-url at a dead local address and
# assert purely on the capture/encode side.
#
# Software encode only (x264enc): CI has no GPU, so this never exercises the
# NVENC/VA-API paths (those are out of scope; see the issue's Out-of-scope).
#
# Scope of the assertion (intentionally a liveness/startup smoke, not an
# endurance test): the agent must reach BOTH
#   "First frame captured from X display"   (real X capture happened)
#   "First H.264 frame from encoder"        (real GStreamer encode happened)
# within a bounded timeout. We do NOT gate on a sustained N-frame heartbeat —
# that turns a startup smoke into an endurance smoke and adds flakiness on
# shared CI runners for little extra signal. Frame-rate / heartbeat / audio /
# resize assertions are deferred follow-ups.
#
# Exit non-zero (loudly, with the agent + Xvfb logs) on any failure. There is
# deliberately no `|| true` masking anywhere on the assertion path — a smoke
# that cannot fail proves nothing.

set -euo pipefail

DISPLAY_NUM="${BEAM_SMOKE_DISPLAY:-99}"
DISPLAY_STR=":${DISPLAY_NUM}"
SCREEN_W="${BEAM_SMOKE_WIDTH:-1280}"
SCREEN_H="${BEAM_SMOKE_HEIGHT:-720}"
# Existing-display mode captures at the X screen's real size, so width/height
# below are passed for completeness but the Xvfb screen geometry is what the
# capture actually uses.
AGENT_BIN="${BEAM_AGENT_BIN:-./target/release/beam-agent}"
# A deterministic, well-formed UUID — beam-agent requires --session-id to be a
# valid UUID, but the value is irrelevant to the capture/encode smoke.
SESSION_ID="${BEAM_SMOKE_SESSION_ID:-00000000-0000-4000-8000-000000000131}"
# Intentionally dead WebSocket endpoint: the capture+encode thread runs
# regardless of whether the agent can reach a server. Port 1 is privileged and
# never listening, so the connect fails fast and the agent just retries while
# capture/encode proceeds.
SERVER_URL="${BEAM_SMOKE_SERVER_URL:-ws://127.0.0.1:1}"
ASSERT_TIMEOUT="${BEAM_SMOKE_TIMEOUT:-45}" # seconds to see both markers
XVFB_READY_TIMEOUT="${BEAM_SMOKE_XVFB_TIMEOUT:-15}" # seconds for Xvfb to bind
# How many times to (re)launch the agent if it dies immediately with the
# X-connection-reset startup signature. A transient reset is a startup race,
# not a real fault, so we retry a small bounded number of times before
# declaring failure. A genuine capture/encode crash (any other signature)
# fails on the FIRST attempt — it is never retried.
CONNECT_RETRIES="${BEAM_SMOKE_CONNECT_RETRIES:-3}"

AGENT_LOG="$(mktemp /tmp/beam-agent-smoke.XXXXXX.log)"
XVFB_LOG="$(mktemp /tmp/beam-xvfb-smoke.XXXXXX.log)"
# Xvfb writes its display number here once it is fully ready to accept client
# connections (the -displayfd readiness signal — see the startup block below).
XVFB_READY_FILE="$(mktemp /tmp/beam-xvfb-ready.XXXXXX)"
XVFB_PID=""
AGENT_PID=""

CAPTURE_MARKER="First frame captured from X display"
ENCODE_MARKER="First H.264 frame from encoder"
# Substring that identifies the Xvfb-startup race we retry on: the agent's X
# client connection is reset right after a readiness probe passed. Matched
# against the agent log to distinguish this transient from a real fault.
RESET_SIGNATURE="Failed to connect to X display"

log() { printf '[agent-smoke] %s\n' "$*"; }

dump_logs() {
  log "----- beam-agent log (${AGENT_LOG}) -----"
  cat "${AGENT_LOG}" 2>/dev/null || log "(no agent log captured)"
  log "----- Xvfb log (${XVFB_LOG}) -----"
  cat "${XVFB_LOG}" 2>/dev/null || log "(no Xvfb log captured)"
  log "----------------------------------------"
}

cleanup() {
  if [ -n "${AGENT_PID}" ] && kill -0 "${AGENT_PID}" 2>/dev/null; then
    kill "${AGENT_PID}" 2>/dev/null || true
    wait "${AGENT_PID}" 2>/dev/null || true
  fi
  if [ -n "${XVFB_PID}" ] && kill -0 "${XVFB_PID}" 2>/dev/null; then
    kill "${XVFB_PID}" 2>/dev/null || true
    wait "${XVFB_PID}" 2>/dev/null || true
  fi
  rm -f "${AGENT_LOG}" "${XVFB_LOG}" "${XVFB_READY_FILE}"
}
trap cleanup EXIT

fail() {
  log "SMOKE FAILED: $*"
  dump_logs
  exit 1
}

# --- preconditions ---------------------------------------------------------
[ -x "${AGENT_BIN}" ] || fail "beam-agent binary not found/executable at ${AGENT_BIN}"
command -v Xvfb >/dev/null 2>&1 || fail "Xvfb not installed"
command -v xdpyinfo >/dev/null 2>&1 || fail "xdpyinfo not installed (x11-utils)"
# xclip is mandatory: ClipboardBridge::new() shells out to `which xclip` and
# the agent aborts startup (before the capture thread) if it is absent.
command -v xclip >/dev/null 2>&1 || fail "xclip not installed (agent aborts at clipboard init without it)"

# Surface the GStreamer element availability explicitly BEFORE booting the
# agent, so a missing runtime plugin fails with a clear message rather than an
# opaque pipeline-construction error buried in the agent log.
if command -v gst-inspect-1.0 >/dev/null 2>&1; then
  for elem in appsrc appsink videoconvert x264enc h264parse; do
    gst-inspect-1.0 "${elem}" >/dev/null 2>&1 \
      || fail "required GStreamer element '${elem}' not available (missing plugin package?)"
  done
  log "GStreamer elements present: appsrc appsink videoconvert x264enc h264parse"
else
  log "WARN: gst-inspect-1.0 not found; skipping explicit element pre-check"
fi

# --- start the virtual display ---------------------------------------------
log "Starting Xvfb on ${DISPLAY_STR} (${SCREEN_W}x${SCREEN_H}x24)..."
# -ac disables host-based access control (no xauth dance needed for a
# throwaway CI display); -nolisten tcp keeps it off the network.
#
# -displayfd is the readiness fix. A one-shot xdpyinfo probe can succeed while
# the server is still finishing its startup, so the agent's *next* client
# connection gets reset milliseconds later on a loaded runner — the race this
# script exists to be immune to. -displayfd makes Xvfb itself write its display
# number to the given fd ONLY after it has finished initialization and is
# accepting connections in its event loop. We pass an explicit display number,
# so the value written back is just that number; we use the write purely as the
# "server is now truly ready" edge, not to discover the display. fd 3 is
# redirected to a file we then poll for content — robust under set -e and a
# backgrounded server.
Xvfb "${DISPLAY_STR}" -screen 0 "${SCREEN_W}x${SCREEN_H}x24" -ac -nolisten tcp \
  -displayfd 3 3>"${XVFB_READY_FILE}" >"${XVFB_LOG}" 2>&1 &
XVFB_PID=$!

# Block until Xvfb signals readiness via -displayfd (a non-empty ready file),
# with a bounded timeout. This is a true server-emitted readiness edge, not a
# client-side probe that can pass before the server is stable.
log "Waiting up to ${XVFB_READY_TIMEOUT}s for ${DISPLAY_STR} to signal ready (-displayfd)..."
deadline=$(( $(date +%s) + XVFB_READY_TIMEOUT ))
until [ -s "${XVFB_READY_FILE}" ]; do
  if ! kill -0 "${XVFB_PID}" 2>/dev/null; then
    fail "Xvfb exited during startup"
  fi
  if [ "$(date +%s)" -ge "${deadline}" ]; then
    fail "timed out after ${XVFB_READY_TIMEOUT}s waiting for Xvfb on ${DISPLAY_STR}"
  fi
  sleep 0.1
done
# Belt-and-braces: one client probe AFTER the server-emitted ready edge. If
# this fails the display is genuinely broken (not the startup race), so fail
# loud rather than launch the agent into a dead display.
if ! DISPLAY="${DISPLAY_STR}" xdpyinfo >/dev/null 2>&1; then
  fail "Xvfb signaled ready on ${DISPLAY_STR} but xdpyinfo could not connect"
fi
log "Xvfb ready on ${DISPLAY_STR} (display $(cat "${XVFB_READY_FILE}") via -displayfd)"

# --- boot beam-agent + assert capture/encode, with bounded connect-retry ----
# One launch+assert pass. Returns:
#   0  -> both markers seen (success)
#   2  -> agent died before any marker WITH the X-connect-reset signature
#         (a transient startup race — the caller may retry)
# Any genuine fault (agent dies with a different signature, dies AFTER making
# capture/encode progress, or the markers time out) calls fail() directly and
# exits non-zero — those are never retried, so a real regression still fails
# loud and fast.
run_agent_once() {
  : >"${AGENT_LOG}" # fresh log per attempt so signature/marker greps are clean
  log "Starting beam-agent against ${DISPLAY_STR} (software x264enc)..."
  RUST_LOG="${RUST_LOG:-info}" "${AGENT_BIN}" \
    --display "${DISPLAY_STR}" \
    --server-url "${SERVER_URL}" \
    --session-id "${SESSION_ID}" \
    --encoder x264enc \
    --width "${SCREEN_W}" \
    --height "${SCREEN_H}" \
    --framerate 30 \
    --bitrate 2000 \
    >"${AGENT_LOG}" 2>&1 &
  AGENT_PID=$!

  log "Waiting up to ${ASSERT_TIMEOUT}s for capture + encode markers..."
  local deadline saw_capture=0 saw_encode=0
  deadline=$(( $(date +%s) + ASSERT_TIMEOUT ))
  while :; do
    if ! kill -0 "${AGENT_PID}" 2>/dev/null; then
      wait "${AGENT_PID}" 2>/dev/null || true
      AGENT_PID=""
      # The agent exited. Only the Xvfb-startup connection race is retryable,
      # and only if it happened BEFORE any capture/encode progress — a death
      # after real progress is a genuine fault.
      if [ "${saw_capture}" -eq 0 ] && [ "${saw_encode}" -eq 0 ] \
         && grep -qF "${RESET_SIGNATURE}" "${AGENT_LOG}"; then
        log "agent exited at X-connect with the startup-race signature ('${RESET_SIGNATURE}')"
        return 2
      fi
      fail "beam-agent exited before reaching capture+encode (crash on startup?)"
    fi
    if [ "${saw_capture}" -eq 0 ] && grep -qF "${CAPTURE_MARKER}" "${AGENT_LOG}"; then
      saw_capture=1
      log "OK: capture marker seen ('${CAPTURE_MARKER}')"
    fi
    if [ "${saw_encode}" -eq 0 ] && grep -qF "${ENCODE_MARKER}" "${AGENT_LOG}"; then
      saw_encode=1
      log "OK: encode marker seen ('${ENCODE_MARKER}')"
    fi
    if [ "${saw_capture}" -eq 1 ] && [ "${saw_encode}" -eq 1 ]; then
      return 0
    fi
    if [ "$(date +%s)" -ge "${deadline}" ]; then
      fail "timed out after ${ASSERT_TIMEOUT}s (capture=${saw_capture} encode=${saw_encode})"
    fi
    sleep 0.5
  done
}

attempt=1
while :; do
  if run_agent_once; then
    break
  fi
  # run_agent_once returned 2: the retryable startup-race signature. Any other
  # outcome already exited via fail().
  if [ "${attempt}" -ge "${CONNECT_RETRIES}" ]; then
    fail "beam-agent hit the X-connect startup race on all ${CONNECT_RETRIES} attempts"
  fi
  backoff=$(awk "BEGIN { printf \"%.2f\", 0.25 * ${attempt} }")
  log "retrying agent launch (attempt $((attempt + 1))/${CONNECT_RETRIES}) after ${backoff}s backoff..."
  sleep "${backoff}"
  attempt=$((attempt + 1))
done

log "AGENT DISPLAY SMOKE PASSED (capture + encode pipeline live against ${DISPLAY_STR})"
