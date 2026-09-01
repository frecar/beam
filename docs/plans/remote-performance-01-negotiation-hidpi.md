# Plan 01 — Capability, DPR, and stream-state negotiation

## Goal

Send Retina/HiDPI clients a framebuffer sized for physical display intent instead of a CSS-sized framebuffer that the browser enlarges, while bounding encoder/decoder/network cost and making the effective result visible.

## Decision

Use:

`desired pixels = CSS content size × requested DPR`

Then choose the largest even-sized result satisfying all of:

- client-advertised decode/canvas limits;
- server `max_width`/`max_height`;
- a new max pixel budget;
- configured max DPR (default proposal: `2.0`);
- codec alignment;
- current congestion-controller render scale.

Preserve aspect ratio while clamping. Report `effective_dpr = encoded_width / CSS_width` (and the height equivalent) and the limiting reason. Do not hard-code DPR 1 for software encoding: benchmark DPR 1.0/1.5/2.0 on the current host and let the configured pixel cap enforce what it can sustain.

A default 8.3-megapixel (4K) cap is a reasonable starting ceiling, not an acceptance decision. The current deployment can retain a lower cap until measurements prove higher settings sustainable.

## Negotiation contract

### Client hello/auth capability data

Add validated optional fields alongside the current viewport fields:

- content width/height in CSS pixels;
- `device_pixel_ratio`;
- screen and visual viewport data needed to explain browser zoom;
- maximum preferred decode width/height/pixels;
- supported codec/profile candidates from `VideoDecoder.isConfigSupported()`;
- WebCodecs-in-worker and OffscreenCanvas support;
- WebTransport/datagram support;
- protocol/header versions.

Capability fields are hints, not trusted authorization or allocation values. Limit lengths/counts and clamp all numerics server-side.

Old clients that send only `viewport_width`/`viewport_height` retain DPR 1 behavior.

### Effective stream descriptor

Introduce a server/agent/browser control message containing:

- stream generation;
- codec and profile/level;
- encoder backend;
- media/control transports;
- encoded width/height;
- CSS width/height and effective DPR;
- FPS and bitrate targets;
- active render scale and limiting reason;
- enabled treatment profile.

Emit an update whenever these values change. The F9 overlay, session panel, and copied stats use this descriptor instead of hard-coded `H.264` and `WSS` labels.

### Runtime changes

`ResizeObserver`, fullscreen transitions, browser zoom changes, and DPR changes when moving between monitors must all re-negotiate. Listen to resolution media-query changes or re-read DPR on resize/focus because `devicePixelRatio` has no universal dedicated event.

Debounce requests, but compare desired physical dimensions—not only CSS dimensions. Replace the current 10% gate with a policy that catches DPR-only changes and avoids resize loops. Include a request generation and require the server’s effective descriptor acknowledgement.

## Implementation slices

1. **Pure sizing policy**
   - Add a shared set of test vectors for CSS size, DPR, pixel cap, dimension cap, codec alignment, and render scale.
   - Implement independently in one authoritative protocol/service location; browser only requests intent.
2. **Initial login**
   - Update `AuthRequest`, `web/src/login.ts`, `crates/server/src/web.rs`, and session creation.
   - Pass effective initial dimensions to `spawn_agent`.
3. **Runtime resize intent**
   - Replace `{t:'r',w,h}` semantics or add a new compatible message carrying CSS size/DPR.
   - Update `web/src/input.ts`, `resize.ts`, agent dispatch, and xrandr resize.
4. **Capabilities**
   - Probe asynchronously before login where possible; cache only for the page lifetime.
   - Re-probe actual codec configurations before switching and handle runtime failure.
5. **Descriptor/UI**
   - Add descriptor plumbing and display actual active settings.
6. **Adaptive hook**
   - Allow plan 07 to change render scale without changing CSS desktop geometry.

## Important distinction

Canvas backing-store size and remote framebuffer size are separate:

- Offscreen/visible canvas backing store matches encoded frame dimensions.
- CSS size matches the content viewport.
- Pointer mapping continues to normalize against actual rendered video bounds.
- Browser page zoom must not be multiplied twice.

## Test plan

- Unit matrices for DPR 1/1.25/1.5/2/3, odd dimensions, 4K pixel cap, browser zoom, monitor changes, max dimension clamp, and render scales.
- Login API backward-compatibility tests with fields absent.
- Resize tests where CSS dimensions remain constant but DPR changes.
- Reconnect test where a different client capability set takes over an existing session.
- Real-browser tests on Retina and DPR-emulated Chromium using `agent-browser`; manually verify Safari/Firefox fallback where automation cannot expose WebCodecs support accurately.
- Ensure pointer coordinates and screenshots remain correct at non-integer effective DPR.

## Benchmark cells

On the current software host, run CSS viewport 1512×800 at effective DPR 1.0, 1.5, and 2.0 (subject to caps), with static text, scroll, and motion. Record sharpness/OCR, encode CPU, bitrate, frame age, and FPS. Repeat at 1920×1080 CSS with clamping to validate the cap reason.

## Acceptance criteria

- A DPR 2 client receives approximately twice the linear framebuffer resolution when limits allow.
- The client displays requested and effective DPR/resolution plus any clamp reason.
- No allocation exceeds validated width, height, or pixel budgets.
- DPR-only monitor transitions complete without reconnect and without resize loops.
- Current x264 production profile has a documented sustainable DPR/pixel cap from benchmark data.
- Old clients and unsupported browsers retain H.264 Main/WSS at DPR 1.

## Dependencies and rollout

Depends on plan 00’s descriptor/manifest conventions. Ship negotiation in observe-only mode first: calculate and display desired/effective values while still allocating DPR 1. Then enable physical-pixel sizing per experiment profile, followed by bounded auto mode.
