# Plan 02 — Dependency-safe frame lifecycle

## Goal

Eliminate smearing/corruption caused by dropping reference H.264 frames, and establish dependency metadata needed by decoder backpressure, datagrams, FEC, and temporal/disposable frames.

## Current hazards

Arbitrary drops occur in at least four places:

- encoded handoff (`encoded_tx.try_send`);
- shared agent WebSocket outbox (`ws_tx.try_send`);
- server broadcast lag;
- future browser `decodeQueueSize` pressure.

With the current IPPP structure, every P-frame must be treated as dependent and potentially referenced. Continuing to send later P-frames after any unknown P-frame drop is unsafe.

## Safety invariant

Unless the encoder explicitly proves a frame disposable:

1. any dropped/unknown video frame invalidates the current dependency chain;
2. enter `AwaitingRecovery`;
3. discard all deltas;
4. request one keyframe (coalesced, rate-limited but not blocked by the five-second pipeline-reset cooldown);
5. resume only after a valid keyframe for the current stream generation is admitted;
6. ensure stale queued frames from the old generation cannot precede/follow recovery.

Never “proceed with P-frames” after exhausting reset attempts. That current fallback trades a visible stream for a predictably undecodable one. Prefer a surfaced stalled/recovering state and retry/reconnect policy.

## Frame model

Add an encoded-frame envelope used internally and on the extended wire header:

- generation and sequence;
- codec;
- `Key`, `Reference`, or verified `Disposable` dependency class;
- temporal ID when the encoder actually exposes one;
- capture timestamp;
- encoded payload;
- recovery reason/epoch where applicable.

H.264 Annex-B parsing can identify IDR, SPS, and PPS, but must not infer disposable status from NAL type alone. Use encoder metadata or a validated codec configuration.

## Queue policy

Replace plain bounded `mpsc<Vec<u8>>` at video boundaries with a video-aware bounded queue:

- control/audio use independent queues;
- queue admission understands generation and dependency class;
- on overflow, atomically clear invalid deltas, mark recovery required, and retain at most a valid keyframe/start of the newest generation;
- keyframes must not sit behind stale deltas;
- emit drop reason/count/high-water metrics;
- one recovery request remains outstanding until fulfilled or timed out.

Do not increase queue capacities to hide pressure; that increases frame age.

At the server, a lagged media consumer triggers the same recovery state, not a generic visibility toggle. Add an explicit `RequestKeyframe { reason, generation }` control message.

At the browser, decoder flush/reset or an unsafe local drop requests recovery and rejects deltas until the matching keyframe arrives.

## Implementation slices

1. **Frame classification**
   - Generalize `crates/agent/src/h264.rs` into codec frame metadata helpers.
   - Add realistic Annex-B fixtures for IDR/SPS/PPS and malformed access units.
2. **Recovery state machine**
   - Implement a pure, exhaustive state transition table.
   - Cover startup, overflow, reconnect, resize, codec switch, timeout, and duplicate requests.
3. **Agent queues**
   - Replace arbitrary drop points in `main.rs` and `video.rs`.
   - Separate media/control queue policies without waiting for transport plan 06.
4. **Server relay**
   - Propagate sequence/generation and explicit recovery requests.
   - Reject old-generation frames.
5. **Browser decoder guard**
   - Correlate decoder state with generation and sequence.
   - Never submit deltas after a known gap unless marked disposable.
6. **Observability**
   - Add recovery reason, gap, requested sequence, keyframe latency, and recovered sequence metrics.

## Fault-injection tests

- Drop each position in a 120-frame IPPP GOP, one at a time.
- Fill every bounded queue deliberately.
- Lag the server broadcast consumer.
- Reorder old/new generation frames.
- Lose a keyframe and selected keyframe fragments.
- Trigger resize/reconnect during recovery.
- Trigger repeated overflows faster than keyframes can be generated.
- Close/recreate `VideoDecoder` while deltas arrive.

Decode the resulting stream and compare output against source. Tests must prove either clean recovery or an explicit frozen/recovering state—never continued corruption.

## Acceptance criteria

- No persistent smear/corruption in any single-drop or queue-overflow injection.
- Recovery keyframe p95 is below 250 ms on the current host under an uncongested path and below one configured recovery deadline under shaped congestion.
- Queue growth is bounded and p95 frame age does not rise as a side effect.
- Recovery requests are coalesced; overload does not produce a keyframe storm.
- Every intentional frame drop has dependency class, reason, stage, generation, and sequence telemetry.
- Existing H.264 Main/WSS clients remain compatible through negotiated old-header behavior.

## Dependencies and rollout

Depends on plan 00 frame identity. This is a prerequisite for browser backpressure, WebTransport datagrams, and FEC. Enable the conservative “all P-frames are reference” policy first; later plan 10 may relax dropping only for encoder-proven disposable frames.
