# Plan 07 — Adaptive congestion and quality control

## Goal

Dynamically adjust bitrate, FPS, and render resolution so Beam minimizes current-frame age and control latency under changing RTT, loss, throughput, and decoder pressure.

## Control objective

Optimize in this order:

1. preserve control/input responsiveness;
2. keep newest-frame age within target;
3. avoid decoder/reassembly/encoder queue growth and corruption;
4. preserve readable resolution;
5. preserve motion FPS;
6. use remaining bandwidth for quality.

This is not a throughput-maximizing controller. A 50 Mbps path with loss/bufferbloat may perform worse than 15 Mbps, as current deployment measurements show.

## Inputs

### Available over WSS

- control RTT/jitter trend;
- received video bytes/FPS;
- sequence gaps/recovery frequency;
- decode queue depth, oldest-frame age, decode duration, output FPS;
- agent queue pressure and encode duration;
- server relay queue/write pressure.

### Added with WebTransport

- datagram sequence loss/reorder/expiry;
- QUIC send pressure and RTT where server APIs expose it;
- reassembly drops and FEC recovery.

Use short-window and long-window EWMAs plus explicit confidence/sample-age. Ignore stale browser reports.

## Controller location and messages

Keep one authoritative per-session controller at the server because it sees client feedback and relay/transport pressure. Implement the decision function as a pure protocol/domain module so it is deterministic and extensively tested.

Server sends typed `StreamControl` updates to agent and an effective stream descriptor to browser. Agent replies with applied/rejected/deferred status because encoder backends differ.

Modes:

- `off`: fixed validated config;
- `observe`: calculate/log recommendations without applying;
- `active`: apply bounded actions.

## Adaptation ladder

Use continuous bitrate within each discrete quality rung. Proposed rungs for experimentation:

- render scales: 1.0, 0.75, 0.5 (relative to negotiated HiDPI intent);
- FPS ceilings: 120, 90, 60, 45, 30, 20;
- bitrate: configured min/target/max and encoder-safe bounds.

Current software host starts at a maximum 60 fps. Damage-driven plan 05 can send fewer frames while preserving that ceiling.

### Reaction policy

- **Fast decrease:** on queue age, sustained RTT inflation, loss/gaps, decoder pressure, or repeated recovery; reduce bitrate first, then FPS for motion overload, then resolution for persistent bandwidth/decoder limits.
- **Slow increase:** only after a stable window with low queue age/loss and resource headroom; increase one dimension at a time.
- Use hysteresis, minimum dwell times, and cooldowns to prevent oscillation.
- Resolution changes are expensive (xrandr + encoder/decoder generation) and therefore least frequent.
- Keyframe cost is budgeted into transitions.
- User/profile min/max limits are hard constraints.

Do not infer “network congestion” from low FPS alone; disambiguate encoder, decoder, static-damage suppression, and transport causes using plan 00 stages.

## Encoder/application mechanics

- Bitrate update: apply live when backend supports it; otherwise rebuild at a safe generation/keyframe boundary.
- FPS: change scheduler ceiling without lying in caps/timestamps; update encoder caps only if required.
- Resolution/render scale: server computes bounded dimensions via plan 01, sends generation transition, drains old frames, resizes xrandr, recreates encoder, and resumes on keyframe.
- If an action fails, keep last known-good settings and report reason; do not stack additional changes.

## Implementation slices

1. Extend browser feedback with queue age/depth, sequence gaps, presentation FPS, and report age.
2. Add server/agent pressure metrics and typed control acknowledgements.
3. Implement pure controller in `observe` mode with recorded decisions.
4. Add live bitrate application.
5. Add scheduler FPS changes.
6. Add infrequent render-scale/resolution transitions.
7. Integrate WebTransport loss/reassembly/FEC signals.
8. Tune thresholds from benchmark traces, not hand-selected anecdotes.

## Deterministic simulation tests

Replay synthetic traces for:

- bandwidth step 50→10→50 Mbps;
- RTT inflation without loss;
- 1–3% burst/random loss;
- slow encoder with clean network;
- slow decoder with clean network;
- static desktop (low FPS is intentional);
- transient keyframe spike;
- stale/missing metrics;
- oscillating capacity near a rung threshold.

Assert bounded actions, dwell times, no oscillation, correct bottleneck classification, and eventual recovery.

## End-to-end benchmark

Run fixed versus observe versus active under scripted capacity/loss transitions. Primary metrics:

- p95/p99 presentation-submit frame age;
- time above age target;
- control RTT inflation;
- visual quality/readability;
- number of quality transitions and recovery keyframes;
- time to reduce after degradation and time to cautiously recover.

## Acceptance criteria

- During a 20→8 Mbps capacity drop, active mode returns p95 frame age below target within 3 seconds without corruption.
- During recovery, quality increases without more than one rung oscillation per 30 seconds.
- Control RTT inflation is lower than fixed high-bitrate mode.
- Decoder pressure is relieved before stale-frame age exceeds the configured hard limit.
- Static damage-driven sessions do not cause the controller to lower quality merely because few frames are sent.
- Every action and fallback is visible in telemetry and effective stream descriptor.
- `off` mode reproduces fixed baseline behavior.

## Dependencies and rollout

Depends on plans 00–03. Begin observe-only over WSS, validate recommendations against traces, then enable bitrate-only adaptation. Add FPS, then resolution. Integrate WebTransport-specific signals after plan 06 without replacing fallback inputs.
