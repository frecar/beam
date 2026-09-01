# Plan 09 — GPU-native capture and DMA-BUF

## Goal

Remove avoidable GPU→CPU→GPU framebuffer copies on supported GPU hosts and improve multi-session scalability, without weakening the current X11 SHM software fallback.

## Priority and feasibility gate

The current production host uses x264, which requires CPU-visible frames; GPU-native capture is therefore deferred until representative NVIDIA and Intel/AMD hosts exist.

Do not choose an implementation before a measured feasibility spike. Headless Xorg root-window capture differs by driver, and “DMA-BUF support” at one boundary does not prove an end-to-end zero-copy path into a specific encoder.

## Baseline copy map

Current path:

1. Xorg/GPU renders root framebuffer;
2. MIT-SHM `GetImage` exposes CPU memory;
3. Beam copies full BGRA into a pooled `Vec`;
4. Beam loops over every pixel to normalize alpha;
5. `GstBuffer::from_slice` wraps CPU memory;
6. conversion/upload may move data back to GPU for NVENC/VAAPI.

Instrument bytes/time for each observable step before replacing it.

## Feasibility spike

Evaluate and document, per driver/host:

- X11 DRI3/root pixmap export to DMA-BUF and whether the virtual display’s root pixmap is exportable;
- GStreamer GL/Vulkan/DMA-BUF capture paths and caps negotiation;
- NVIDIA-specific capture APIs available under project licensing/distribution constraints;
- PipeWire DMA-BUF capture suitability for a service-owned headless X11 session (not assumed from Wayland desktop behavior);
- direct NVENC CUDA memory and VAAPI surface import paths;
- format/modifier compatibility (BGRA/XRGB/NV12/P010), synchronization fences, and multi-session behavior.

For each candidate, prove an end-to-end frame reaches the browser before scoring it. Reject paths requiring an unavoidable CPU readback that perform worse than SHM.

## Capture backend abstraction

Introduce a backend trait/enum with explicit frame ownership:

- `ShmCpu` — current reliable fallback;
- GPU-native backend(s) selected only after probes;
- frame dimensions/format/color metadata;
- CPU frame or GPU/DMA-BUF handles;
- damage/region metadata where available;
- synchronization/fence lifetime;
- backend capability report.

Encoder input accepts typed frame memory rather than always `PooledFrame<Vec<u8>>`. Keep allocation pools per memory type and bound in-flight surfaces.

## GStreamer integration

For DMA-BUF paths:

- wrap file descriptors with correct `GstMemory`/video metadata ownership;
- negotiate `memory:DMABuf` or backend-native caps through conversion and encoder;
- avoid `videoconvert` if it forces CPU mapping;
- use GPU-native color conversion only when measured;
- close duplicated FDs and release surfaces exactly once after downstream completion;
- honor explicit/implicit synchronization requirements;
- report any fallback mapping/upload.

“Pipeline links successfully” is not proof of zero-copy. Verify with memory caps, tracing, CPU copy counters, and profiler evidence.

## Alpha/color handling

Current NVIDIA BGRA path sets alpha byte-by-byte because X11 BGRx padding is undefined. A GPU path should negotiate XRGB/BGRx semantics or perform conversion in GPU without a full CPU alpha pass. Preserve current browser compatibility tests around colorimetry/VUI; DMA-BUF work must not reintroduce rejected color metadata.

## Implementation slices

1. Add baseline capture/copy/upload instrumentation.
2. Build standalone feasibility probes for one NVIDIA and one VAAPI host.
3. Select one backend with explicit go/no-go report.
4. Introduce capture/encoder memory abstraction while retaining SHM behavior.
5. Integrate native surfaces and lifecycle tests.
6. Combine with XDamage scheduling from plan 05.
7. Run multi-session resource/scalability benchmarks.

## Test plan

- Frame correctness for color, stride, crop, odd/even dimensions, and resize.
- FD/surface ownership tests, forced pipeline errors, reconnect, and repeated backend recreation.
- Capability probe false-positive tests and automatic SHM fallback.
- GPU reset/device loss and encoder session exhaustion.
- 1/2/4/8 concurrent sessions where hardware permits.
- Compare CPU profiles and memory bandwidth against SHM at 1080p60/120 and 4K60.
- Mixed damage/static/full-motion workloads.

## Acceptance criteria

- Selected hardware path removes the full-frame CPU copy and alpha loop, demonstrated by profiler/counters rather than naming.
- Capture-to-encoder-submit p95 improves by at least 30% or total agent CPU drops by at least 25% at equal output quality/FPS.
- Two or more concurrent GPU sessions scale materially better than SHM without unbounded surfaces or encoder starvation.
- No visual/color regression and no frame age increase.
- Unsupported, failed, or exhausted GPU paths automatically select `ShmCpu` and report the reason.
- Current software host behavior remains unchanged.

## Dependencies and rollout

Requires plan 00 instrumentation and representative GPU hardware. Plan 05 should land first so static desktops do not exercise an expensive native path unnecessarily. Ship backend probes before the backend itself; enable per-host only after burn-in.
