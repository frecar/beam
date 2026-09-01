# Plan 03 — H.264 bitrate, profile, and encoder-quality tuning

## Goal

Improve text and motion quality with low architectural risk, beginning with the current x264 1080p60 host, then validating H.264 High and hardware AQ/presets on NVENC and VAAPI.

## Immediate configuration correction

`config/beam-production.toml` still specifies 5 Mbps/60 fps while repository defaults are 50 Mbps/120 fps and the measured current deployment is sustainable around 15 Mbps/60 fps. The future implementation should stop shipping a misleading 5 Mbps production template.

Do not simply make 50 Mbps universal. Proposed starting profiles:

- software/WAN: measured 15 Mbps target, 4–20 Mbps bounds, 60 fps;
- hardware/LAN: 50 Mbps target, measured bounds, up to 120 fps;
- auto: hardware- and path-aware selection from plan 07.

All units remain explicit (`kbps`) and are validated consistently. Fix the stale `beam-agent --help` default at the same time.

## Current-host x264 experiment

Measured data shows `veryfast` can exceed 100 fps on the current 4-vCPU host for a synthetic 1080p60 workload, while Beam currently selects `ultrafast`. Benchmark, in order:

1. `ultrafast` baseline at 10/15/20 Mbps;
2. `superfast` at 15 Mbps;
3. `veryfast` at 15 Mbps;
4. `faster` only if end-to-end CPU and frame-age headroom remains.

Keep `tune=zerolatency`, no B-frames, and bounded buffering. Evaluate GOP/keyframe interval separately from preset so recovery cost is attributable. Synthetic throughput alone is not sufficient; run real desktop workloads and concurrent audio/input.

## H.264 High profile

### Negotiation

- Probe `VideoDecoder.isConfigSupported()` for exact High/Main codec strings at intended dimensions/level.
- Prefer High only when the browser advertises support and an actual first-keyframe decode succeeds.
- Keep Main as fallback and retain dynamic SPS-derived codec strings.
- Agent reports actual SPS profile/level; browser verifies it matches the descriptor.

### Encoder changes

Make profile `auto|main|high` configurable. Do not force High globally. Ensure parser emits SPS/PPS with each recovery keyframe. Validate level selection for 4K/120 combinations instead of retaining the hard-coded `avc1.4d0033` fallback.

Compare Main versus High at equal bitrate and equal quality. High is successful only if compression gains outweigh compatibility or encode/decode regressions.

## NVENC/VAAPI quality tuning

Hardware work begins only when representative hosts exist. Build backend-specific property adapters based on runtime GStreamer property introspection; plugin versions expose different names and valid enums.

Evaluate:

- spatial and temporal AQ where supported;
- less aggressive quality presets that preserve zero reorder and bounded latency;
- CBR versus constrained VBR where low-delay VBV remains bounded;
- QP bounds without starving complex motion;
- one-to-three frame VBV options;
- GOP/keyframe interval and intra-refresh support;
- avoiding B-frames/reordering unless an experiment proves no latency regression.

For each applied property, log effective value. Unsupported requested quality properties must produce a clear fallback reason, not a silent no-op.

VAAPI `target-usage=7` is currently the fastest/lowest-quality end. Sweep toward quality while measuring headroom. NVENC AQ and preset names must be derived from installed plugin capabilities, not copied blindly between `nvh264enc` and `nvcudah264enc`.

## Dynamic bitrate control surface

Refactor `Encoder` to expose supported live controls:

- update bitrate/VBV without pipeline rebuild where the element permits;
- report whether a property change was applied live, requires keyframe, or requires rebuild;
- serialize changes on the capture/encoder owner thread;
- expose applied bitrate in the stream descriptor.

This surface is used by plan 07. It does not implement adaptation itself.

## Implementation slices

1. Correct config/help/README examples and validations.
2. Introduce typed codec profile and encoder quality config.
3. Benchmark and select an x264 preset for the current host.
4. Add exact client profile capability negotiation and fallback.
5. Add runtime encoder-property discovery/adapters.
6. Add live bitrate-control API and reason-coded application results.
7. Add hardware matrices when hosts are available.

## Test plan

- Config/default/unit consistency tests across protocol, CLI, examples, and package config.
- Pipeline tests parse actual SPS and assert requested/actual profile.
- Browser tests for High accepted, High rejected, first-decode failure, and Main fallback.
- Property-adapter tests from captured `gst-inspect` fixtures for each supported plugin generation.
- Overload tests ensure slower presets drop/recover safely under plan 02 rather than building latency.
- Visual corpus comparisons at equal bitrate and equal host CPU budget.

## Acceptance criteria

### Current software host

- `veryfast` or selected preset sustains 1080p60 for every workload with p95 capture-to-presentation-submit no more than 5 ms above baseline.
- CPU retains at least 25% measured headroom at p95 and no audio/input starvation occurs.
- At 15 Mbps, text/scroll quality improves materially in edge/OCR and visual review versus `ultrafast`, with no corruption.
- The shipped production example no longer defaults to 5 Mbps.

### H.264 High/hardware

- High provides a measured quality or bitrate benefit and cleanly falls back to Main.
- Applied NVENC/VAAPI settings are observable and unsupported settings are explicit.
- No selected hardware setting introduces frame reordering or unbounded lookahead.

## Dependencies and rollout

Requires plan 02 before increasing drop pressure with slower presets, and plan 00 for attribution. Ship current-host x264 tuning independently. Keep High and each hardware quality adapter behind negotiated experiment profiles until browser/hardware matrices pass.
