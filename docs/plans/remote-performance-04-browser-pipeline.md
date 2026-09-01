# Plan 04 — Browser worker, OffscreenCanvas, and decode backpressure

## Goal

Move video transport parsing, decode, and presentation away from the browser main thread; keep media state bounded so the desktop shown is current rather than a delayed queue of old frames.

## Target architecture

A dedicated media worker owns:

- media WebSocket or WebTransport receive path;
- binary header/datagram parsing and reassembly;
- `VideoDecoder` configuration and generation state;
- decode backpressure/recovery policy;
- an `OffscreenCanvas` transferred from the visible canvas;
- sampled per-frame timing and 1 Hz aggregate metrics.

The main thread owns:

- input event capture and control transport;
- UI, session state, clipboard/file controls;
- visible canvas element and canvas transfer lifecycle;
- audio initially (AudioContext/worklet constraints differ from video workers);
- low-rate diagnostic updates only.

Do not proxy every video frame through main-thread `postMessage`. The worker should own the media connection where supported. During the intermediate WSS phase, transfer `ArrayBuffer`s rather than cloning them if the connection cannot yet move.

## Low-latency presentation

- Request `canvas.getContext('2d', { alpha: false, desynchronized: true })` where supported; record whether the hint was honored only through observed behavior, not API claims.
- Draw each selected `VideoFrame` immediately in decoder output and close it in a `finally` path.
- Bound pending presentation to the newest complete decodable frame.
- Evaluate `bitmaprenderer` only as a benchmark treatment; extra bitmap conversion can negate gains.
- Do not rely on non-portable `commit()` behavior.
- Keep CSS sizing on main; backing size follows the effective stream descriptor.
- Treat presentation submit as the last measurable stage, not physical scanout.

## `decodeQueueSize` policy

Use two watermarks, measured rather than guessed (starting experiment: low=1, high=3):

1. below low: feed normally;
2. between low/high: stop accepting redundant/disposable work and report pressure;
3. above high for a short deadline: apply plan 02 dependency safety.

For current IPPP H.264, do **not** discard one queued P-frame and continue. Instead:

- stop feeding new deltas;
- reset/flush decoder as appropriate;
- request a recovery keyframe;
- reject deltas until matching generation/keyframe;
- resume at the newest safe point.

After plan 10 provides encoder-proven disposable frames, only those may be selectively skipped without reset.

Also track `frames_received - frames_output`, oldest queued frame age, decode callback delay, and worker event-loop lag. `decodeQueueSize` alone does not include every browser-internal/presentation queue.

## Capability/fallback behavior

Probe all required pieces independently:

- WebCodecs in `DedicatedWorkerGlobalScope`;
- `transferControlToOffscreen` and worker 2D context;
- selected codec configuration;
- worker WebSocket/WebTransport support.

Modes:

1. full worker receive/decode/present;
2. worker decode/present with main-thread transport and transferred buffers;
3. current main-thread renderer fallback.

A worker crash or decoder fatal error attempts one clean worker recreation with a new generation/keyframe, then falls back to main-thread H.264/WSS. Display the active browser pipeline mode.

## Module boundaries

Split the current `web/src/webcodecs-renderer.ts` responsibilities into testable modules:

- codec/stream generation state machine;
- media frame parser;
- backpressure policy;
- worker protocol types;
- worker video renderer;
- main-thread fallback renderer;
- audio renderer;
- metrics aggregator.

Avoid high-frequency UI state. Worker sends aggregate diagnostics at 1 Hz and exceptional state transitions immediately. This follows the relevant transient-state principle from `vercel-react-best-practices`, although Beam does not use React.

## Implementation slices

1. Extract pure generation/backpressure state and tests without behavior change.
2. Add worker protocol and bundling entrypoint.
3. Transfer canvas and decode fixture frames in worker.
4. Move WSS media receive into worker; keep controls on main.
5. Add low-latency context treatments and newest-safe-frame presentation.
6. Add crash/fatal-decode fallback.
7. Integrate WebTransport from plan 06.

## Test plan

- Unit-test all queue watermark, gap, generation, keyframe, and fatal-error transitions.
- Feed bursts faster than decode and assert bounded queue/age.
- Stall main thread with deterministic long tasks; worker FPS/frame age should remain stable and input control should remain responsive.
- Stall worker/decode and verify conservative recovery with no corruption.
- Test resize/codec switch/reconnect while frames are queued.
- Test worker unavailable, OffscreenCanvas unavailable, codec rejected, worker crash, and runtime decoder failure.
- Use `agent-browser` performance traces and scripted scrolling/input for real Chromium runs; run Playwright compatibility tests for fallback state/UI.

## Acceptance criteria

- Main-thread long-task time during video workloads decreases by at least 50% on the reference client.
- Under a 100 ms injected main-thread stall, worker mode does not accumulate more than the configured queue bound and input events continue over control transport.
- p95 oldest-frame age is lower than main-thread baseline under scroll/motion.
- No resource leak across 20 reconnect/resize cycles; every `VideoFrame`, decoder, worker, and canvas lifecycle is closed once.
- Decode overload recovers without smearing and without presenting stale pre-generation frames.
- Unsupported clients transparently use and display the main-thread fallback.

## Dependencies and rollout

Depends on plans 00–02 and plan 01’s stream descriptor. Land extraction first, then worker mode behind a client capability/treatment switch. Make worker mode default only after real-browser fallback and long-run reconnect tests pass.
