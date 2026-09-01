# Remote streaming performance experiment roadmap

Status: proposed

Tracking: GitHub issue #263

Scope: planning only; this roadmap does not authorize implementation or deployment.

## Decisions and constraints

- Preserve **H.264 over WebSocket** as the compatibility path for Firefox, Safari, and clients that fail capability or runtime probes.
- Treat **H.264 High, HEVC, worker rendering, and WebTransport datagrams** as negotiated enhancements, never assumptions.
- Show the effective codec/profile, encoder, media transport, control transport, resolution, effective DPR, FPS target, bitrate target, and active experiment profile in the session information and F9 diagnostics.
- HTTP/3/WebTransport may listen on UDP on the configured HTTPS port. TCP HTTPS/WSS remains available on that same numeric port.
- Optimize and benchmark the current software-encoded 1920x1080@60 deployment first. Add NVENC and VAAPI lanes when representative hardware is available.
- HiDPI uses physical-pixel intent (`CSS pixels × DPR`) but is bounded by server resolution, pixel-budget, decoder, and congestion limits. The server returns the effective DPR rather than pretending the request was fully honored.
- Every treatment must be independently selectable and emit an effective configuration manifest so benchmark results can be attributed.
- No treatment becomes the default merely because it increases throughput. It must also pass latency, corruption, fallback, resource, and reconnect gates.

## Current-state findings

- `web/src/login.ts` and `web/src/input.ts` send CSS-pixel dimensions and omit `devicePixelRatio`, so Retina clients can receive a low-resolution framebuffer that CSS then enlarges.
- The repository defaults to 50 Mbps/120 fps, but `config/beam-production.toml` still selects 5 Mbps/60 fps. The current deployment config uses 15 Mbps/60 fps with x264.
- `crates/agent/src/main.rs` and `crates/agent/src/video.rs` drop arbitrary encoded frames when bounded queues fill. A dropped reference P-frame invalidates later dependent frames until recovery.
- The encoder forces H.264 Main and uses `x264enc` `ultrafast`, while measured host capacity in the deployment benchmark leaves room for `veryfast` at 1080p60.
- Video, audio, clipboard, cursor, and file data share one agent WebSocket outbox. Browser control and binary media share one ordered browser WebSocket.
- Capture reads and encodes a full X11 framebuffer on every pacing tick, including unchanged frames.
- Browser WebSocket parsing, WebCodecs decode, canvas draw, UI, and input handling all run on the main thread. `decodeQueueSize` is not consulted.
- Existing browser metrics provide RTT, FPS, rough decode delay, bytes, and drop totals, but not per-frame capture-to-presentation stage timing.

## Plans and dependency order

| Order | Plan | Primary outcomes | Depends on |
| --- | --- | --- | --- |
| 0 | [Measurement and experiment framework](./remote-performance-00-measurement.md) | Stage telemetry, reproducible workloads, treatment manifest, comparison report | None |
| 1 | [Capability, DPR, and stream-state negotiation](./remote-performance-01-negotiation-hidpi.md) | Correct Retina framebuffer intent, bounded DPR, explicit effective mode in UI | Plan 0 schema conventions |
| 2 | [Dependency-safe frame lifecycle](./remote-performance-02-safe-frame-lifecycle.md) | No corruption after queue pressure; sequence/generation metadata and recovery | Plan 0 telemetry |
| 3 | [H.264 bitrate, profile, and encoder-quality tuning](./remote-performance-03-h264-quality.md) | Fix 5 Mbps template, evaluate x264 presets, High profile, NVENC/VAAPI AQ | Plans 0–2 |
| 4 | [Browser worker, OffscreenCanvas, and decode backpressure](./remote-performance-04-browser-pipeline.md) | Keep media work off main thread; bounded decoder age | Plans 0–2 and negotiated state from plan 1 |
| 5 | [Damage-driven capture and XDamage](./remote-performance-05-damage-capture.md) | Stop duplicate-frame encoding while retaining burst FPS | Plans 0 and 2 |
| 6 | [Separate control/media paths and WebTransport](./remote-performance-06-webtransport.md) | Input unaffected by media congestion; stale datagrams expire | Plans 0–2 and plan 1 capabilities |
| 7 | [Adaptive congestion and quality control](./remote-performance-07-adaptation.md) | Adjust bitrate/FPS/render scale from measured pressure | Plans 0–3; integrate plan 6 feedback when available |
| 8 | [HEVC negotiated codec support](./remote-performance-08-hevc.md) | Hardware HEVC quality-per-bit enhancement with H.264 fallback | Plans 0–3 and plan 1 negotiation |
| 9 | [GPU-native capture and DMA-BUF](./remote-performance-09-gpu-capture.md) | Remove GPU→CPU→GPU copies where hardware permits | Plan 0; representative GPU hardware |
| 10 | [Datagram FEC and disposable temporal frames](./remote-performance-10-resilience.md) | Recover isolated loss and permit proven-safe selective dropping | Plans 2, 6, and codec capability work |

Plans 4 and 5 can proceed in parallel after frame safety. Plan 7 can begin in observe-only mode over WebSocket before plan 6 lands. Plans 8–10 should not delay improvements for the current software host.

## Experiment controls

Implementation should expose explicit, validated configuration rather than hidden constants. The exact schema is finalized in plan 0, but it must cover:

- negotiated codec/profile and media transport preferences;
- target/min/max bitrate, FPS ceiling, render scale, DPR cap, and pixel cap;
- encoder preset/AQ controls per encoder backend;
- safe-drop policy and recovery interval;
- worker presentation and decode-queue limits;
- fixed-rate versus damage-driven capture;
- congestion controller `off`, `observe`, and `active` modes;
- FEC overhead and temporal-layer policy.

Each session emits a machine-readable effective manifest. Unsupported values fail validation or explicitly fall back with a reason; they must not silently disappear.

## Benchmark method

1. Establish the unchanged baseline on the current x264 host.
2. Run one-factor-at-a-time treatments using identical desktop and network traces.
3. Repeat each measured cell at least five times after warm-up.
4. Run static desktop, terminal/text scroll, browser scroll, window drag, full-screen motion, and input-latency workloads.
5. Test clean LAN plus shaped 10/20/50 Mbps paths at 10/50/100 ms RTT and 0/0.1/1/3% random loss.
6. Capture p50/p95/p99 frame age, input delivery, FPS, queue depth, recovery time, bitrate, CPU, RSS, and visual-quality metrics.
7. Reject any treatment with persistent smearing, decode failure, control starvation, fallback failure, or unbounded queues.
8. Test selected interaction bundles only after individual effects are known.
9. Publish effect sizes and confidence intervals, not only the best single run.

The durable benchmark table remains append-only. Existing rows are never overwritten by newer treatments.

## Cross-cutting acceptance gates

- A browser with no new capability fields still connects using H.264 Main over WSS.
- A client that advertises but fails an enhanced runtime path automatically returns to the fallback without requiring a new login.
- Input/control p95 does not regress under saturated media traffic.
- The browser never intentionally presents an older frame when a newer complete frame is already available.
- Every queue has a documented bound, overflow policy, metric, and dependency-safe recovery action.
- Resolution, codec, profile, and transport transitions use generation IDs so stale packets cannot enter a new decoder generation.
- Config examples, `beam-agent --help`, diagnostics, and README are updated in the same future implementation changes that alter operator-facing behavior.
- Tests include both negotiation success and forced fallback paths.

## Relevant skills

- Use the `agent-browser` skill for real-browser capability probes, diagnostics verification, main-thread responsiveness checks, and repeatable interaction workloads. Load its live `core` workflow before running it.
- The `vercel-react-best-practices` skill was consulted for client performance principles such as moving transient high-frequency state out of UI rendering and avoiding main-thread contention. Beam is vanilla TypeScript rather than React, so React-specific component rules do not apply.
- No available skill covers GStreamer, QUIC/WebTransport, XDamage, codec dependency graphs, or DMA-BUF. Those plans therefore rely on repository architecture, backend capability probes, and measured experiments rather than pretending a UI skill is authoritative.
