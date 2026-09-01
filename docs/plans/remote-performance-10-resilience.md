# Plan 10 — Datagram FEC and disposable temporal frames

## Goal

Recover occasional Wi-Fi datagram loss without retransmission and allow selected encoded frames to be dropped only when the codec/encoder proves they are non-reference/disposable.

## Preconditions

- Plan 02 defines frame generation, sequence, dependency safety, and conservative recovery.
- Plan 06 defines bounded datagram packetization/reassembly and stale deadlines.
- Codec plans expose real frame metadata.

Until all three are true, every inter frame remains `Reference` and any loss triggers recovery.

## Lightweight FEC design

Start with single-parity XOR FEC over small groups of datagram payloads. One parity packet can recover one missing packet in a group with low CPU and bounded delay.

### Grouping policy

- Group fragments from the same frame whenever possible; never hold fragments across a frame deadline merely to fill a group.
- Include original lengths and packet indexes in authenticated headers so a short final packet can be reconstructed.
- Initial experiment: groups of 8 data + 1 parity (~12.5% overhead), then sweep 4/8/16 and lower overhead.
- Adapt overhead only among explicit validated settings (off/low/medium/high); do not build a second opaque congestion controller.
- Cap decoder/reassembly allocation before processing parity.

### Keyframe policy

Benchmark separately:

1. FEC-protected keyframe datagrams;
2. smaller FEC groups/stronger parity for keyframes;
3. keyframes/config on a reliable WebTransport stream with delta datagrams.

A reliable keyframe can itself be delayed by loss/retransmission, so do not select option 3 without frame-age and recovery evidence.

### FEC accounting

Report packets sent/lost/recovered/unrecoverable, parity bytes, recovery delay, frame saved/dropped, and post-FEC effective loss. A recovered packet received after frame deadline counts as late/unusable, not success.

## Temporal/disposable frame structure

### Safety rule

A frame is `Disposable` only when encoder-produced metadata or a validated bitstream/codec configuration guarantees no future admitted frame references it. Configuration intent alone is insufficient.

### Candidate structures

Evaluate backend support for:

- H.264 temporal layers/hierarchical P with no B-frame reorder;
- HEVC temporal sublayers on supported hardware;
- encoder reference-picture invalidation or long-term-reference recovery features;
- periodic keyframes/intra refresh as recovery tools (not disposable-frame claims).

A conceptual two-layer pattern might alternate base reference frames with enhancement/disposable frames, but exact reference structure must be verified by encoder metadata and decode fault injection. Hardware backends may differ; unsupported encoders stay single-layer conservative IPPP.

### Transport/drop policy

- Base/reference frames receive higher priority and optional stronger FEC.
- Disposable enhancement frames may expire/drop under send, reassembly, or decoder pressure without forcing recovery.
- Browser can lower temporal ceiling under load.
- Sequence gaps still record which dependency class was lost.
- Never mark a frame disposable by sequence parity or temporal ID unless that ID’s reference contract is validated for the active encoder configuration.

## Implementation slices

1. Add FEC header fields reserved by plan 06 and pure XOR codec.
2. Add bounded sender grouping and receiver recovery.
3. Benchmark fixed FEC levels under random and burst loss.
4. Add observe-only FEC recommendation hook to plan 07.
5. Probe temporal-layer controls/metadata per encoder.
6. Add verified dependency classifier and conformance fixture.
7. Enable selective disposable dropping at one stage at a time (sender, network expiry, decoder pressure).
8. Combine FEC priority with base/enhancement layers.

## Test plan

### FEC

- Recover every single missing position for variable packet lengths.
- Reject two missing packets for XOR without corrupt reconstruction.
- Reorder, duplicate, truncate, corrupt header, cross-generation, and expire groups.
- Fuzz parser/allocation bounds.
- Loss traces: 0/0.1/1/3/5% random and burst losses from recorded Wi-Fi conditions.

### Temporal structure

- Drop every purported disposable frame individually and in bursts; later reference frames must decode identically.
- Drop every base/reference frame and verify plan 02 recovery.
- Parse/verify metadata for each encoder/plugin fixture.
- Force backend to ignore requested temporal settings; classifier must remain conservative.
- Exercise decoder backpressure selecting only enhancement drops.

## Benchmark matrix

Compare:

- WSS fallback;
- WebTransport without FEC;
- WebTransport with each FEC group/overhead;
- single-layer conservative recovery;
- verified temporal layers without FEC;
- temporal layers plus priority FEC.

Measure frame age, usable presented FPS, recovery freezes, quality, bytes including parity, CPU, and control RTT. Score net benefit at each loss rate; FEC is expected to hurt clean-path bandwidth slightly.

## Acceptance criteria

- At 1% random packet loss, selected FEC recovers at least 70% of otherwise-lost single-packet groups before deadline and reduces visible recovery events materially.
- FEC CPU overhead is below 3% agent/server/browser each at 1080p60 on reference systems.
- Clean-path frame-age regression is below 1 ms p95 and overhead matches configured bounds.
- Every frame labeled disposable passes exhaustive drop-without-downstream-corruption tests for that encoder configuration.
- Under decoder/send pressure, dropping disposable frames lowers stale age without increasing keyframe requests.
- Unsupported temporal encoders remain conservative and fully functional.

## Dependencies and rollout

FEC can ship after WebTransport independently of temporal layers. Start fixed and opt-in, then let plan 07 choose among measured levels. Temporal dropping remains disabled per backend until its conformance suite passes on the exact plugin/driver family.
