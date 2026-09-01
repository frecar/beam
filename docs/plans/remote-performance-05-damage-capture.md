# Plan 05 — Damage-driven capture and XDamage

## Goal

Retain up to 120 fps during desktop activity while avoiding full capture/encode of duplicate static frames. Use XDamage as the primary activity signal and dirty-region source, with correctness fallbacks.

## Phased design

### Phase A — duplicate suppression baseline

Before XDamage, add an experiment that detects unchanged content at a cheaper stage only if measurement proves it cheaper than encoding. Full-frame CPU hashing after the SHM copy may save encode work but not capture/copy cost, so treat it as a temporary baseline, not the destination.

### Phase B — XDamage-driven scheduling

Enable the XDamage extension in `x11rb` and create a dedicated event connection/thread for the root window:

- subscribe to damage events;
- coalesce bursts until the next target frame deadline;
- wake the capture thread through the existing condition-variable mechanism;
- capture immediately on transition from no damage to damage;
- cap active output at configured FPS;
- stop periodic active capture when no damage is pending;
- still emit bounded keepalive/recovery frames as required by codec/transport state.

Input wakes the scheduler but is not itself proof that pixels changed. Damage events trigger actual capture; use a short input anticipation deadline to reduce input-to-visual delay without encoding endless unchanged frames.

### Phase C — dirty-region capture

Track the union/list of damaged rectangles and benchmark sub-rectangle `MIT-SHM GetImage` updates into a persistent full-frame backing buffer. Normalize alpha only within updated rows. The encoder still receives a full frame unless ROI/partial-update support is explicitly available, but this can reduce X11 read/copy work for small changes.

Do not assume one giant union is optimal; cap rectangle count and switch to full-frame capture when damaged area or fragmentation exceeds thresholds.

## Correctness safeguards

XDamage may miss or behave differently across drivers/compositors. Add:

- startup extension probe and fixed-rate fallback;
- periodic full refresh (experiment starting at 1 second, configurable);
- full refresh after resize, reconnect, keyframe request, capture error, or generation change;
- sequence counters for damage received/coalesced/captured;
- watchdog that compares occasional full frames in validation mode to detect missed updates;
- explicit behavior when desktop compositor is disabled (current XFCE default) and for dummy/NVIDIA Xorg.

Cursor pixels are intentionally hidden and cursor shape is sent separately, so local cursor motion should not force video frames. Verify this rather than assuming it on every Xorg driver.

## Scheduler states

A pure state machine should cover:

- `Static`: wait for damage/input/recovery deadline;
- `Burst`: damage pending, capture at active FPS ceiling;
- `Coalescing`: another damage event arrived before next deadline;
- `RefreshDue`: periodic/keyframe/full refresh overrides static state;
- `Background`: preserve existing 1 fps policy or lower if keepalive semantics allow;
- `FallbackFixedRate`: extension unavailable/watchdog failed.

Idle based only on “no input for five minutes” becomes secondary. Desktop activity (animations, remote process output) must sustain frames even without local input.

## Implementation slices

1. Add capture/scheduler counters and deterministic fake event source.
2. Add XDamage probe, subscription, and wake integration.
3. Replace active fixed tick with damage-driven state machine.
4. Add periodic full refresh and fallback/watchdog.
5. Add region coalescing and partial SHM update treatment.
6. Later expose encoder ROI hints only when a backend proves support and quality benefit.

Likely files: `crates/agent/Cargo.toml`, `capture.rs`, `main.rs`, new damage/scheduler module, diagnostics, and capture readiness checks.

## Test plan

- Unit-test scheduler timing, burst coalescing, no-input remote animation, background/foreground, and recovery override.
- Fake damage rectangles: overlap, off-screen, empty, highly fragmented, full-screen, resize race.
- Integration test under Xvfb/Xorg fixture where drawing a rectangle emits damage and updates only expected pixels.
- Static desktop soak: verify near-zero encoded FPS while input/control remain alive.
- Continuous scroll: verify configured active FPS and no coalescing-induced stutter.
- Terminal output without input: verify it is not classified static.
- Force extension failure/watchdog mismatch and verify fixed-rate fallback.

## Benchmark cells

Compare fixed-rate, full-frame hash suppression, XDamage scheduling, and XDamage+region updates for:

- 60-second static desktop;
- cursor-only movement;
- one blinking/animated region;
- terminal scroll;
- full-screen motion.

Measure captures, copied bytes, encoded frames, CPU, bitrate, frame age, and input-to-first-damaged-frame latency.

## Acceptance criteria

- Static desktop capture/encode count drops by at least 95% versus fixed 60 fps, excluding configured refresh frames.
- Static agent CPU and bitrate fall materially without increasing control RTT.
- Continuous motion sustains the target FPS within 5% when encoder capacity permits.
- Input-to-first-visible-update p95 does not regress by more than one active frame interval.
- No missed-update watchdog failures in an eight-hour mixed-workload soak.
- XDamage-unavailable clients automatically use the fixed-rate path and report the fallback.

## Dependencies and rollout

Requires plan 00 telemetry and plan 02 recovery behavior. Ship XDamage scheduling separately from region capture so their effects are measurable. Keep fixed-rate fallback permanently for unsupported/broken X servers.
