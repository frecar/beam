# Plan 06 — Separate control/media paths and WebTransport datagrams

## Goal

Prevent media congestion or loss from delaying keyboard/mouse/control traffic, then remove TCP head-of-line blocking for video by carrying packetized media over WebTransport datagrams.

## Architecture decision

Use independent logical and physical paths:

- **Control:** reliable ordered WSS initially (input, keyframe requests, stream descriptors, clipboard metadata, session lifecycle). It has its own small queue and write task.
- **Media fallback:** separate WSS for H.264/Opus, preserving current browser compatibility.
- **Enhanced media:** WebTransport over HTTP/3/QUIC; video uses datagrams. Control remains on WSS in the first release so a media experiment cannot strand input.
- **Agent→server:** separate control and media connections/queues. Because agent and server normally share a host, a separate TCP/Unix media path is acceptable initially; browser-leg QUIC is the WAN-critical part.

Large file transfer and clipboard payloads must not share the latency-critical input queue. Put them on a bulk reliable stream/path with explicit flow control.

The server listens on TCP and UDP at the same configured numeric port. TLS certificate identity and authentication policy are shared, but HTTP/3/QUIC lifecycle is independently observable.

## Phase 1 — separation while retaining WSS

Before QUIC:

1. split browser control and media WebSockets/endpoints;
2. split agent control/media outboxes and writer tasks;
3. give control strict bounded priority without sharing a socket write blocked on a large binary frame;
4. classify audio separately from video and bulk data;
5. authenticate each connection with a short-lived, session-bound, purpose-bound token;
6. retain one-connection compatibility mode for old clients.

This phase measures the value of separation independently from datagrams.

## Phase 2 — WebTransport session

Add an HTTP/3/WebTransport server alongside the existing axum/hyper TCP server (likely QUIC/H3-specific crates rather than forcing axum to own unsupported HTTP/3 details).

Negotiation flow:

1. browser capability hello advertises WebTransport/datagrams;
2. server returns a short-lived media token and endpoint;
3. browser opens WebTransport, waits for `ready`, and sends authenticated session binding on a reliable stream;
4. server confirms generation/codec/packetization limits;
5. video starts on datagrams;
6. if setup or health probe fails, browser returns to media WSS without new login.

Bind tokens to session, user, transport purpose, expiry, and nonce. Enforce replay protection and datagram rate/size limits.

## Datagram packetization

Encoded frames exceed path MTU and must be fragmented. Define a compact packet header containing:

- protocol/header version;
- session-safe connection identifier (not a secret);
- stream generation;
- frame sequence;
- fragment index/count or byte offset/total length;
- frame dependency class and temporal ID from plan 02;
- keyframe/config marker;
- capture timestamp delta;
- FEC group fields reserved for plan 10;
- integrity covered by QUIC (no redundant custom encryption).

Start with a conservative maximum datagram payload around 1200 bytes, then use negotiated QUIC maximum datagram size. Reject impossible fragment counts/frame sizes before allocation.

Browser reassembly is bounded by:

- max frames in flight;
- max bytes;
- per-frame deadline derived from frame age target;
- newest generation/sequence;
- dependency safety.

Expire incomplete stale frames instead of requesting retransmission. A lost unsafe reference frame enters plan 02 recovery. Keyframe strategy (datagram with stronger protection versus reliable stream) is benchmarked before selection.

## Audio decision

Benchmark Opus on:

- reliable media stream/WSS; and
- datagrams with a small jitter buffer.

Do not couple video’s “drop stale” policy to audio automatically. Audio frames are independently decodable but playback continuity has different goals.

## Network/operations work

- Add UDP listener and package/firewall documentation for the configured port.
- Advertise HTTP/3/Alt-Svc only when listener health is confirmed.
- Ensure reverse-proxy docs state HTTP/3/WebTransport requirements and fallback behavior.
- Add QUIC connection/session limits, idle timeout, key logging prohibition, and metrics.
- Extend post-deploy checks to verify TCP fallback and optionally UDP enhancement without making fallback health depend on UDP.

## Test plan

- Unit-test packet serialize/parse, limits, wraparound, malformed headers, generation rejection, and reassembly expiry.
- Integration-test separate WSS control/media under a blocked media writer.
- QUIC tests for handshake/auth/replay/expiry, loss, reorder, duplication, MTU variation, NAT rebinding, and fallback.
- Inject 1–3% loss and verify datagrams avoid stale retransmitted frames while recovery remains clean.
- Saturate video and assert keyboard/mouse control latency remains bounded.
- Test browser without WebTransport, UDP blocked, HTTP/3 unavailable behind proxy, and mid-session QUIC failure.

## Acceptance criteria

- Under saturated media, control p95 server→agent delivery is below 20 ms above path RTT and no input is queued behind a media frame.
- At 1% random loss, WebTransport mode has lower p95 frame age than media WSS and no persistent corruption.
- Incomplete frames are removed by deadline and memory remains within configured bounds.
- UDP-blocked and unsupported clients fall back to separate media WSS automatically.
- F9/session info clearly shows `Control: WSS` and `Media: WebTransport datagrams` or the actual fallback.
- Existing single-WSS clients remain functional during compatibility window.

## Dependencies and rollout

Depends on plans 00–02 and plan 01 capability negotiation. Ship queue/socket separation first. Add WebTransport as opt-in, then auto-preferred after fallback and network-path coverage. Plan 10 adds FEC/temporal policy without redesigning packet identity.
