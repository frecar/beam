# Plan 00 — Measurement and experiment framework

## Goal

Make every later performance change attributable. Measure capture→encode→relay→receive→decode→presentation-submit timing, queue pressure, recovery, visual quality, and host cost using repeatable workloads.

## Scope

- A versioned per-frame metadata contract with sequence and stream-generation IDs.
- Monotonic timestamps at each observable stage.
- Browser/server/agent clock-offset estimation where stages cross clocks.
- A bounded telemetry sampler and opt-in Prometheus/report output.
- Reproducible desktop and network workloads.
- Independent treatment profiles and a machine-readable effective manifest.
- A comparison report ranked by effect size.

Actual photon/display scanout is not observable in a normal browser. Name the final metric `presentation_submit`, not “glass,” and document that it ends when the decoded frame is submitted to the canvas.

## Design

### Frame identity and timing

Extend the binary media contract compatibly (new negotiated header version or extension block) with:

- `stream_generation`: increments for reconnect, codec/profile change, decoder reset, and resolution change;
- `frame_sequence`: monotonically increasing within a generation;
- `capture_timestamp`;
- frame kind/dependency metadata from plan 02;
- optional sampled timing extension carrying encode-complete and agent-send timestamps.

Record server ingress/egress locally without rewriting every payload. Browser records socket/datagram receive, decode enqueue, decoder output, and canvas submission. Use microseconds and monotonic clocks internally.

Estimate browser↔server offset with an NTP-style four-timestamp exchange and report uncertainty. Agent and server normally share a host but must still exchange monotonic anchors rather than assume matching clock epochs. Never calculate a cross-clock duration when offset uncertainty exceeds the sample’s expected duration; expose it as unavailable.

Sample stage-rich metadata (for example 1 in 60 frames plus all keyframes/recoveries) to avoid inflating every frame. Sequence/generation IDs remain on every frame.

### Metrics

Per session/treatment:

- frame age at browser receive, decoder enqueue, decoder output, and presentation submit;
- capture, encode, agent queue, server relay, network, decode, and presentation-submit spans where measurable;
- inter-frame arrival and presentation jitter;
- queue depths/high-water marks: appsrc, encoded handoff, agent media outbox, server relay, reassembly, `decodeQueueSize`, pending presentation;
- captured/encoded/sent/received/decoded/presented/dropped counts by reason;
- keyframe request count and time-to-recovery;
- control RTT and input server→agent delivery delay under media load;
- actual bitrate and frame size by frame kind;
- host CPU/RSS, per-process CPU, and GPU encoder/utilization when available;
- effective resolution, DPR, codec/profile, encoder, transport, FPS, and treatment manifest.

Do not overload the existing `decode_ms`, which currently measures from latest feed call to callback and can be overwritten by queued frames. Replace it with sequence-correlated samples.

### Visual-quality corpus

Create deterministic scenes:

1. static terminal and IDE text;
2. vertical terminal scroll;
3. browser page scroll with text and images;
4. window drag over a detailed background;
5. full-screen motion/video;
6. rapid typing and pointer movement;
7. static desktop for duplicate-frame accounting.

Capture the uncompressed source and decoded output for offline SSIM/VMAF plus text-focused edge/OCR comparison. Keep the source, duration, viewport, font scaling, and random seed fixed. Visual corruption remains an automatic failure even if aggregate quality scores look good.

### Treatment manifest

Define a serializable `ExperimentManifest` assembled from validated effective settings, not requested settings. Include a stable treatment ID, Beam commit/version, browser build, host class, encoder/plugin versions, config knobs, fallback reasons, network profile, and workload ID.

Support one-variable profiles and named bundles. Prevent accidental comparisons when more than the allowed variable set differs. Do not permit unauthenticated browser query parameters to override server performance/security settings.

## Implementation slices

1. **Protocol foundation**
   - Update `crates/protocol/src/frame.rs` and `web/src/connection.ts` parsing.
   - Add generation/sequence round-trip and malformed-extension tests.
   - Preserve header-version fallback behavior.
2. **Clock synchronization**
   - Add control messages in `crates/protocol/src/messages.rs`.
   - Implement offset/uncertainty calculation as pure tested functions.
3. **Stage instrumentation**
   - Instrument `crates/agent/src/main.rs`, `encoder.rs`, `video.rs`, server relay, `connection.ts`, and renderer/worker.
   - Add bounded reason-coded counters.
4. **Effective stream descriptor**
   - Emit descriptor updates to browser and logs.
   - Display current mode in F9/session information and copied stats.
5. **Benchmark runner**
   - Add scripts for workload orchestration, `tc netem`, process metrics, and artifact collection.
   - Keep network shaping opt-in and restore host state on every exit path.
6. **Report generator**
   - Produce raw JSON/CSV and a Markdown summary with medians, tails, confidence intervals, and regressions.

## Test plan

- Unit-test header extensions, sequence rollover policy, generation rejection, clock math, sampling, and manifest canonicalization.
- Integration-test timestamp propagation through agent→server→browser fixtures.
- Inject known delays at each queue and verify attribution lands on that stage.
- Verify absent/invalid timing extensions do not block fallback clients.
- Verify telemetry disabled mode adds negligible payload and no periodic reports.
- Use `agent-browser` for real-browser diagnostics and input responsiveness checks; use existing Playwright only for committed deterministic e2e guards.

## Acceptance criteria

- At least 95% of sampled frames in a clean benchmark have a valid capture-to-presentation-submit breakdown or an explicit uncertainty reason.
- Added unsampled per-frame overhead stays below 32 bytes; sampled overhead and sampling rate are reported.
- Telemetry CPU overhead is below 2% relative on the current host and does not change p95 frame age by more than 1 ms.
- All drops are classified by stage/reason; “unknown drop” is zero in controlled tests.
- A baseline report can be reproduced within ±10% for median bitrate/FPS/CPU across five runs.
- F9/session info states what codec/profile, encoder, transport, effective DPR/resolution, FPS, bitrate, and experiment profile are actually active.

## Dependencies and rollout

This is the first plan. Land protocol fields dark, then instrumentation, then benchmark tooling. Keep client metrics opt-in. No adaptive controller may consume a metric until delay injection has validated that metric’s semantics.
