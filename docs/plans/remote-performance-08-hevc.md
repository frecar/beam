# Plan 08 — Negotiated HEVC codec support

## Goal

Add hardware HEVC as an optional quality-per-bit enhancement while preserving H.264 Main/WSS for every unsupported browser, encoder, or runtime failure.

## Priority

This is not on the critical path for the current x264 production host. Software HEVC is unlikely to fit its latency/CPU budget. Implement and benchmark when an NVENC or VAAPI host with HEVC low-latency encode is available.

## Capability negotiation

Browser sends exact `VideoDecoder.isConfigSupported()` results for candidate HEVC configurations (`hvc1`/`hev1` form as required by the emitted stream), dimensions, and hardware preference. Because platform/browser support can vary by OS and installed hardware:

- feature-detect; never user-agent gate;
- run an actual short decode/configuration probe before switching where practical;
- cache only per page/session;
- prefer HEVC only if both browser decode and server encode probes pass;
- keep H.264 as immediate fallback.

The UI shows actual `HEVC` plus profile/level, hardware/software decoder indication where reliably known, and fallback reason.

## Agent encoder abstraction

Generalize H.264-specific structures:

- `Codec::{H264, Hevc}` (wire representation stable and explicit);
- encoder candidate detection per codec/backend;
- codec-specific parser/config extraction;
- keyframe/IRAP classification;
- codec-data (VPS/SPS/PPS for HEVC; SPS/PPS for H.264);
- stream format/alignment negotiation;
- profile/level/tier reporting;
- per-codec browser codec string derivation.

Candidate GStreamer elements are probed at runtime (for example NVIDIA and VAAPI HEVC encoders available on the installed plugin set). Do not claim HEVC support because a factory exists; instantiate and encode/decode a probe frame.

## HEVC pipeline requirements

- low-delay mode, no B-frame reordering for the first implementation;
- bounded VBV/lookahead;
- periodic parameter sets with recovery keyframes;
- Annex-B access units or another WebCodecs-compatible format selected explicitly;
- correct IRAP/keyframe detection (IDR/CRA policy documented);
- Main profile first; 10-bit is a separate future experiment and must not silently activate;
- dimensions/alignment validated per backend.

## Stream switching and fallback

Codec changes always increment stream generation and require a clean decoder recreation/keyframe. Never mix H.264 and HEVC payloads under one generation.

Fallback triggers:

- capability probe rejection;
- encoder probe/pipeline failure;
- first keyframe decode failure;
- repeated decoder fatal errors;
- unsupported profile/level;
- active controller deciding hardware/CPU budget cannot sustain HEVC.

On failure, server selects H.264, agent creates H.264 pipeline, browser recreates decoder, and descriptor records the reason. No new login is required.

## Transport interaction

HEVC works over media WSS and WebTransport packetization. Codec support must not depend on WebTransport, allowing codec and transport effects to be benchmarked independently.

Plan 02 conservative dependency safety applies. HEVC NAL type alone is insufficient to declare arbitrary inter frames disposable. Plan 10 handles temporal structure after encoder metadata validation.

## Implementation slices

1. Refactor codec-neutral frame/encoder interfaces with no H.264 behavior change.
2. Add HEVC Annex-B parser and fixtures for VPS/SPS/PPS/IRAP.
3. Add runtime encoder probe and plugin property adapters.
4. Add browser capability/actual-decode probe.
5. Add HEVC WebCodecs configuration in both main-thread and worker renderers.
6. Add generation-safe switch/fallback.
7. Benchmark hardware quality/bitrate/latency.

## Test matrix

- Browsers/OS: current Chrome/Edge, Firefox, Safari on representative platforms; unsupported rows must pass fallback rather than HEVC.
- Encoders: each available NVENC and VAAPI implementation/plugin version.
- Resolutions/FPS: 1080p60 first, then 1440p60, 4K60, and only then higher FPS.
- Transports: media WSS and WebTransport independently.
- Failure cases: missing plugin, instantiate failure, wrong profile, malformed VPS/SPS/PPS, first decode failure, mid-session decoder failure, reconnect/resize.

## Benchmark comparison

At equal bitrate compare H.264 Main, H.264 High, and HEVC. At equal visual quality compare required bitrate. Report encode/decode latency, host/GPU utilization, frame age, keyframe size/recovery, and browser energy/CPU where measurable.

## Acceptance criteria

- HEVC reduces bitrate by at least 25% at comparable text/motion quality, or materially improves quality at the same bitrate, on a supported hardware pair.
- Encode/decode p95 does not increase end-to-end frame age by more than 5 ms at 1080p60.
- Recovery after loss remains clean and within plan 02 deadline.
- Every unsupported or failed matrix row transitions to H.264 automatically without re-login.
- Codec and actual profile/level are visible in diagnostics.
- No software HEVC default is enabled on the current production host.

## Dependencies and rollout

Depends on plan 01 capabilities, plan 02 generations/recovery, and plan 03 encoder controls. Land codec-neutral refactor first. Keep HEVC explicit opt-in until supported hardware/browser matrices pass; then make it `auto`-preferred only for known-good negotiated sessions.
