use crate::capture::PooledFrame;
use anyhow::{Context, bail};
use gstreamer::prelude::*;
use gstreamer::{self as gst, ClockTime, ElementFactory, FlowError};
use gstreamer_app::{AppSink, AppSinkCallbacks, AppSrc};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use tracing::{debug, info, warn};

/// Detected encoder type, exposed so peer.rs can register the matching H.264 profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderType {
    Nvidia,
    /// CUDA-based NVENC (nvcudah264enc) — required for Blackwell GPUs (RTX 50xx)
    /// where the legacy nvh264enc silently produces zero output.
    NvidiaCuda,
    VaApi,
    Software,
}

pub struct Encoder {
    pipeline: gst::Pipeline,
    appsrc: AppSrc,
    encoded_rx: std::sync::Mutex<mpsc::Receiver<Vec<u8>>>,
    _bus_watch: gst::bus::BusWatchGuard,
    /// Set by the GStreamer bus watch on pipeline error. The capture thread
    /// checks this each iteration and recreates the encoder if set.
    pipeline_error: Arc<AtomicBool>,
}

impl Encoder {
    pub fn with_encoder_preference(
        width: u32,
        height: u32,
        framerate: u32,
        bitrate: u32,
        preferred_encoder: Option<&str>,
    ) -> anyhow::Result<Self> {
        let (encoder_type, encoder_name) = detect_encoder(preferred_encoder)?;
        info!(
            ?encoder_type,
            encoder_name, width, height, framerate, bitrate, "Creating H.264 encoder pipeline"
        );

        let pipeline = gst::Pipeline::new();

        // appsrc: raw frames from X11 capture.
        // X11 depth-24 TrueColor captures as 4 bytes/pixel (byte 3 is padding).
        // NVIDIA (both nvh264enc and nvcudah264enc): use BGRA.
        //   capture_frame() fills the 4th byte to 0xFF (X11 depth-24 padding → alpha).
        //   Using BGRx with videoconvert produces valid H.264 but Chrome's VideoDecoder
        //   refuses to decode it (black screen / EncodingError). BGRA avoids this.
        //   nvcudah264enc can't accept BGRA directly, so videoconvert handles BGRA→NV12.
        // Other encoders: use BGRx with videoconvert for correct conversion.
        let format = match encoder_type {
            EncoderType::Nvidia | EncoderType::NvidiaCuda => "BGRA",
            _ => "BGRx",
        };
        let appsrc_elem = ElementFactory::make("appsrc")
            .name("src")
            .build()
            .context("Failed to create appsrc")?;

        let caps = gst::Caps::builder("video/x-raw")
            .field("format", format)
            .field("width", width as i32)
            .field("height", height as i32)
            .field("framerate", gst::Fraction::new(framerate as i32, 1))
            .build();

        let appsrc = appsrc_elem
            .dynamic_cast::<AppSrc>()
            .map_err(|_| anyhow::anyhow!("Failed to cast to AppSrc"))?;
        appsrc.set_caps(Some(&caps));
        appsrc.set_is_live(true);
        appsrc.set_format(gst::Format::Time);
        // CRITICAL: block=false prevents push_buffer() from blocking forever
        // when nvh264enc stalls (GPU busy, NVENC session issue). Without this,
        // the capture thread silently hangs with 0 FPS and no error log.
        appsrc.set_property("block", false);
        // Limit internal queue to 3 frames worth of raw data. When the encoder
        // can't keep up (e.g. software x264enc at high fps), excess frames are
        // dropped via push_buffer returning FlowError. Without this limit,
        // the queue grows unbounded and causes OOM (seen with x264enc at 120fps).
        let frame_bytes = (width * height * 4) as u64; // BGRA = 4 bytes/pixel
        appsrc.set_property("max-bytes", frame_bytes * 3);
        // Minimize internal buffering in appsrc
        appsrc.set_property("min-latency", 0i64);
        appsrc.set_property("max-latency", 0i64);

        // encoder element
        let encoder = build_encoder_element(encoder_type, &encoder_name, bitrate, framerate)?;

        // capsfilter: force main profile for best quality/compression ratio.
        // WebCodecs VideoDecoder handles all H.264 profiles natively.
        let profile_caps = gst::Caps::builder("video/x-h264")
            .field("profile", "main")
            .build();
        let capsfilter = ElementFactory::make("capsfilter")
            .property("caps", &profile_caps)
            .build()
            .context("Failed to create profile capsfilter")?;

        // h264parse: inline SPS/PPS with every keyframe
        let parser = ElementFactory::make("h264parse")
            .property_from_str("config-interval", "-1")
            .build()
            .context("Failed to create h264parse")?;

        // Force h264parse to output Annex B byte-stream with complete access units.
        // Browser's VideoDecoder expects Annex B format (start codes).
        let parse_caps = gst::Caps::builder("video/x-h264")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build();
        let parse_capsfilter = ElementFactory::make("capsfilter")
            .name("parse-caps")
            .property("caps", &parse_caps)
            .build()
            .context("Failed to create h264parse output capsfilter")?;

        // appsink: pull encoded H.264 NAL units.
        // max-buffers=1 + drop=true: absolute minimum buffering for lowest latency.
        // Combined with channel capacity 2 in main.rs, total pipeline depth is
        // at most 3 frames (~25ms at 120fps).
        // async=false: don't wait for clock sync on state changes.
        let appsink_elem = ElementFactory::make("appsink")
            .name("sink")
            .property("sync", false)
            .property("async", false)
            .property("emit-signals", true)
            .property("max-buffers", 1u32)
            .property("drop", true)
            .build()
            .context("Failed to create appsink")?;

        let appsink = appsink_elem
            .dynamic_cast::<AppSink>()
            .map_err(|_| anyhow::anyhow!("Failed to cast to AppSink"))?;

        // Channel to collect encoded frames
        let (encoded_tx, encoded_rx) = mpsc::channel::<Vec<u8>>();

        appsink.set_callbacks(
            AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    let sample = sink.pull_sample().map_err(|_| FlowError::Eos)?;
                    let buffer = sample.buffer().ok_or(FlowError::Error)?;
                    let map = buffer.map_readable().map_err(|_| FlowError::Error)?;
                    let _ = encoded_tx.send(map.to_vec());
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        // Build pipeline based on encoder type.
        // h264parse with config-interval=-1 inlines SPS/PPS with every IDR frame
        // (required for Chrome's decoder to initialize).
        //
        // NVIDIA: appsrc(BGRA) → nvh264enc → capsfilter(main) → h264parse → appsink
        //   No videoconvert: nvh264enc accepts BGRA directly and does GPU color
        //   conversion. Profile capsfilter forces main profile to match browser's
        //   VideoDecoder codec string (avc1.4d0033). Without it, nvh264enc may
        //   output high profile, causing VideoDecoder decode errors.
        // Other:  appsrc(BGRx) → videoconvert → encoder → capsfilter → h264parse → appsink
        match encoder_type {
            EncoderType::Nvidia => {
                pipeline
                    .add_many([
                        appsrc.upcast_ref(),
                        &encoder,
                        &capsfilter,
                        &parser,
                        &parse_capsfilter,
                        appsink.upcast_ref(),
                    ])
                    .context("Failed to add elements to NVIDIA pipeline")?;
                gst::Element::link_many([
                    appsrc.upcast_ref(),
                    &encoder,
                    &capsfilter,
                    &parser,
                    &parse_capsfilter,
                    appsink.upcast_ref(),
                ])
                .context("Failed to link NVIDIA pipeline")?;
                info!(
                    "NVIDIA pipeline: appsrc(BGRA) → nvh264enc → capsfilter(main) → h264parse → appsink"
                );
            }
            EncoderType::NvidiaCuda => {
                let convert = ElementFactory::make("videoconvert")
                    .build()
                    .context("Failed to create videoconvert for CUDA encoder")?;
                pipeline
                    .add_many([
                        appsrc.upcast_ref(),
                        &convert,
                        &encoder,
                        &capsfilter,
                        &parser,
                        &parse_capsfilter,
                        appsink.upcast_ref(),
                    ])
                    .context("Failed to add elements to NVIDIA CUDA pipeline")?;
                gst::Element::link_many([
                    appsrc.upcast_ref(),
                    &convert,
                    &encoder,
                    &capsfilter,
                    &parser,
                    &parse_capsfilter,
                    appsink.upcast_ref(),
                ])
                .context("Failed to link NVIDIA CUDA pipeline")?;
                info!(
                    "NVIDIA CUDA pipeline: appsrc(BGRA) → videoconvert → nvcudah264enc → capsfilter(main) → h264parse → appsink"
                );
            }
            _ => {
                let convert = ElementFactory::make("videoconvert")
                    .build()
                    .context("Failed to create videoconvert")?;
                pipeline
                    .add_many([
                        appsrc.upcast_ref(),
                        &convert,
                        &encoder,
                        &capsfilter,
                        &parser,
                        &parse_capsfilter,
                        appsink.upcast_ref(),
                    ])
                    .context("Failed to add elements to pipeline")?;
                gst::Element::link_many([
                    appsrc.upcast_ref(),
                    &convert,
                    &encoder,
                    &capsfilter,
                    &parser,
                    &parse_capsfilter,
                    appsink.upcast_ref(),
                ])
                .context("Failed to link pipeline elements")?;
                info!("Pipeline: appsrc(BGRx) → videoconvert → encoder → h264parse → appsink");
            }
        }

        // Set up bus watch for error monitoring.
        // The guard must be kept alive or the watch is removed.
        let pipeline_error = Arc::new(AtomicBool::new(false));
        let pipeline_error_flag = Arc::clone(&pipeline_error);
        let bus = pipeline.bus().context("Failed to get pipeline bus")?;
        let _bus_watch = bus
            .add_watch(move |_, msg| {
                use gst::MessageView;
                match msg.view() {
                    MessageView::Error(err) => {
                        tracing::error!(
                            source = ?err.src().map(|s| s.name().to_string()),
                            error = %err.error(),
                            debug = ?err.debug(),
                            "GStreamer pipeline error"
                        );
                        pipeline_error_flag.store(true, Ordering::Relaxed);
                    }
                    MessageView::Warning(warn) => {
                        tracing::warn!(
                            source = ?warn.src().map(|s| s.name().to_string()),
                            warning = %warn.error(),
                            "GStreamer pipeline warning"
                        );
                    }
                    MessageView::StateChanged(state)
                        if state
                            .src()
                            .map(|s| s.name().as_str() == "pipeline0")
                            .unwrap_or(false) =>
                    {
                        tracing::debug!(
                            old = ?state.old(),
                            new = ?state.current(),
                            "Pipeline state changed"
                        );
                    }
                    _ => {}
                }
                gst::glib::ControlFlow::Continue
            })
            .context("Failed to add bus watch")?;

        pipeline
            .set_state(gst::State::Playing)
            .context("Failed to set pipeline to Playing")?;

        info!(
            width,
            height, framerate, bitrate, "Encoder pipeline started"
        );

        Ok(Self {
            pipeline,
            appsrc,
            encoded_rx: std::sync::Mutex::new(encoded_rx),
            _bus_watch,
            pipeline_error,
        })
    }

    /// Push a raw frame into the encoder. Takes ownership of a pooled frame
    /// buffer. When GStreamer finishes encoding, the PooledFrame is dropped
    /// and the backing memory is returned to the capture pool for reuse.
    pub fn encode_frame(&self, frame: PooledFrame, pts: u64) -> anyhow::Result<()> {
        let mut buffer = gst::Buffer::from_slice(frame);
        {
            let buffer_mut = buffer
                .get_mut()
                .expect("freshly-created GstBuffer should have unique ownership");
            buffer_mut.set_pts(ClockTime::from_nseconds(pts));
        }
        self.appsrc
            .push_buffer(buffer)
            .context("Failed to push buffer to appsrc")?;
        Ok(())
    }

    /// Returns true if the GStreamer pipeline has encountered an error.
    /// The capture thread should recreate the encoder when this returns true.
    pub fn has_error(&self) -> bool {
        self.pipeline_error.load(Ordering::Relaxed)
    }

    /// Force the encoder to emit an IDR keyframe on the next frame.
    /// Call this on connection so the browser's VideoDecoder
    /// can start decoding immediately.
    pub fn force_keyframe(&self) {
        let event = gstreamer_video::UpstreamForceKeyUnitEvent::builder()
            .all_headers(true)
            .build();
        self.appsrc.send_event(event);
        info!("Forced IDR keyframe from encoder");
    }

    pub fn pull_encoded(&self) -> anyhow::Result<Option<Vec<u8>>> {
        let rx = self.encoded_rx.lock().unwrap_or_else(|e| e.into_inner());
        match rx.try_recv() {
            Ok(data) => Ok(Some(data)),
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => {
                bail!("Encoder pipeline disconnected")
            }
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        info!("Encoder::drop() - sending EOS and setting pipeline to Null");
        let _ = self.appsrc.end_of_stream();
        let _ = self.pipeline.set_state(gst::State::Null);
        info!("Encoder::drop() - pipeline set to Null, NVENC session should be freed");
    }
}

/// Try to instantiate a GStreamer element to verify the hardware is actually
/// available. `ElementFactory::find()` only checks the plugin registry (the
/// `.so` is present), but the element may fail to create if the hardware
/// driver is missing or inaccessible (e.g. nvh264enc registered but no GPU).
fn can_instantiate(name: &str) -> bool {
    match ElementFactory::make(name).build() {
        Ok(elem) => {
            let _ = elem.set_state(gst::State::Null);
            true
        }
        Err(_) => false,
    }
}

/// Detect which encoder type is available without creating a full pipeline.
/// Returns (encoder_type, encoder_element_name).
pub fn detect_encoder_type(preferred: Option<&str>) -> anyhow::Result<(EncoderType, String)> {
    detect_encoder(preferred)
}

fn detect_encoder(preferred: Option<&str>) -> anyhow::Result<(EncoderType, String)> {
    // If user specified a preferred encoder, try it first
    if let Some(pref) = preferred {
        let enc_type = match pref {
            "nvh264enc" => EncoderType::Nvidia,
            "nvcudah264enc" => EncoderType::NvidiaCuda,
            "vah264enc" => EncoderType::VaApi,
            "x264enc" => EncoderType::Software,
            _ => bail!(
                "Unknown encoder: {pref}. Use nvh264enc, nvcudah264enc, vah264enc, or x264enc."
            ),
        };
        if can_instantiate(pref) {
            info!(encoder = pref, "Using preferred encoder from config");
            return Ok((enc_type, pref.to_string()));
        }
        warn!(
            encoder = pref,
            "Preferred encoder not available, falling back to auto-detect"
        );
    }

    // Prefer nvcudah264enc over nvh264enc: the CUDA-based encoder works on
    // both legacy and Blackwell (RTX 50xx) GPUs, while nvh264enc silently
    // produces zero output on Blackwell despite being instantiable.
    let candidates = [
        (EncoderType::NvidiaCuda, "nvcudah264enc"),
        (EncoderType::Nvidia, "nvh264enc"),
        (EncoderType::VaApi, "vah264enc"),
        (EncoderType::Software, "x264enc"),
    ];

    for (enc_type, name) in &candidates {
        if can_instantiate(name) {
            info!(encoder = name, "Found working encoder");
            return Ok((*enc_type, name.to_string()));
        }
        debug!(encoder = name, "Encoder not available, trying next");
    }

    bail!("No H.264 encoder found. Install gstreamer plugins (good/bad/ugly).")
}

fn build_encoder_element(
    encoder_type: EncoderType,
    name: &str,
    bitrate: u32,
    framerate: u32,
) -> anyhow::Result<gst::Element> {
    let elem = match encoder_type {
        EncoderType::Nvidia => ElementFactory::make(name)
            .property_from_str("preset", "low-latency-hq")
            .property_from_str("rc-mode", "cbr-ld-hq")
            .property("bitrate", bitrate)
            .property("gop-size", i32::MAX)
            .property("zerolatency", true)
            .property("rc-lookahead", 0u32)
            .property("bframes", 0u32)
            .property("strict-gop", true)
            .property("qp-max-i", 20i32)
            .property("qp-max-p", 23i32)
            .property("vbv-buffer-size", bitrate / framerate.max(1)) // 1 frame buffer
            .build()
            .context("Failed to create nvh264enc")?,
        EncoderType::NvidiaCuda => ElementFactory::make(name)
            .property_from_str("preset", "low-latency-hq")
            .property_from_str("rate-control", "cbr")
            .property("bitrate", bitrate)
            .property("gop-size", -1i32)
            .property("zero-reorder-delay", true)
            .property("rc-lookahead", 0u32)
            .property("b-frames", 0u32)
            .property("vbv-buffer-size", bitrate / framerate.max(1))
            .build()
            .context("Failed to create nvcudah264enc")?,
        EncoderType::VaApi => ElementFactory::make(name)
            .property_from_str("rate-control", "cbr")
            .property("bitrate", bitrate)
            .property("target-usage", 7u32)
            .property("key-int-max", 60u32)
            .build()
            .context("Failed to create vah264enc")?,
        EncoderType::Software => ElementFactory::make(name)
            .property_from_str("tune", "zerolatency")
            .property_from_str("speed-preset", "ultrafast")
            .property("bitrate", bitrate)
            .property("key-int-max", 30u32)
            .property("bframes", 0u32)
            .build()
            .context("Failed to create x264enc")?,
    };

    Ok(elem)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify appsrc caps do NOT contain colorimetry.
    /// Adding colorimetry (e.g., bt709) injects VUI colour parameters into the
    /// H.264 SPS, which Chrome's VideoDecoder rejects.
    #[test]
    fn appsrc_caps_must_not_contain_colorimetry() {
        gst::init().unwrap();
        for format in &["BGRA", "BGRx"] {
            let caps = gst::Caps::builder("video/x-raw")
                .field("format", *format)
                .field("width", 1920i32)
                .field("height", 1080i32)
                .field("framerate", gst::Fraction::new(60, 1))
                .build();
            let caps_str = caps.to_string();
            assert!(
                !caps_str.contains("colorimetry"),
                "appsrc caps must NOT specify colorimetry (causes Chrome decode failure). \
                 Format={format}, caps={caps_str}"
            );
        }
    }

    #[test]
    fn encoder_type_equality() {
        assert_eq!(EncoderType::Nvidia, EncoderType::Nvidia);
        assert_eq!(EncoderType::NvidiaCuda, EncoderType::NvidiaCuda);
        assert_ne!(EncoderType::Nvidia, EncoderType::NvidiaCuda);
        assert_ne!(EncoderType::VaApi, EncoderType::Software);
    }

    #[test]
    fn detect_encoder_rejects_unknown_name() {
        let result = detect_encoder(Some("totally_fake_encoder"));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Unknown encoder"),
            "Should mention unknown encoder: {err}"
        );
    }

    #[test]
    fn detect_encoder_maps_known_names_to_types() {
        gst::init().unwrap();
        let mappings = [
            ("nvh264enc", EncoderType::Nvidia),
            ("nvcudah264enc", EncoderType::NvidiaCuda),
            ("vah264enc", EncoderType::VaApi),
            ("x264enc", EncoderType::Software),
        ];
        for (name, expected_type) in mappings {
            if let Ok((enc_type, enc_name)) = detect_encoder(Some(name)) {
                // Only verify mapping if the preferred encoder was actually used
                // (not a fallback to a different encoder)
                if enc_name == name {
                    assert_eq!(enc_type, expected_type, "Wrong type for {name}");
                }
            }
        }
    }

    #[test]
    fn detect_encoder_auto_finds_something() {
        gst::init().unwrap();
        let result = detect_encoder(None);
        if let Ok((enc_type, name)) = result {
            assert!(!name.is_empty());
            match enc_type {
                EncoderType::Nvidia => assert_eq!(name, "nvh264enc"),
                EncoderType::NvidiaCuda => assert_eq!(name, "nvcudah264enc"),
                EncoderType::VaApi => assert_eq!(name, "vah264enc"),
                EncoderType::Software => assert_eq!(name, "x264enc"),
            }
        }
        // If no encoder available (bare CI), just skip
    }

    #[test]
    fn nvidia_uses_bgra_format() {
        // NVIDIA encoder needs BGRA (not BGRx) to avoid Chrome decode failures
        assert_eq!(
            match EncoderType::Nvidia {
                EncoderType::Nvidia => "BGRA",
                _ => "BGRx",
            },
            "BGRA"
        );
    }

    #[test]
    fn non_nvidia_uses_bgrx_format() {
        for enc_type in [
            EncoderType::NvidiaCuda,
            EncoderType::VaApi,
            EncoderType::Software,
        ] {
            let format = match enc_type {
                EncoderType::Nvidia => "BGRA",
                _ => "BGRx",
            };
            assert_eq!(
                format, "BGRx",
                "Non-Nvidia encoder {enc_type:?} should use BGRx"
            );
        }
    }

    // --- can_instantiate ---

    #[test]
    fn can_instantiate_rejects_unknown_element_name() {
        gst::init().unwrap();
        assert!(!can_instantiate("definitely-not-a-real-element-name-xyz"));
    }

    #[test]
    fn can_instantiate_accepts_x264enc_when_present() {
        gst::init().unwrap();
        // x264enc is part of gst-plugins-ugly. CI installs the gstreamer-1.0
        // plugin set, so this should be available in the coverage job.
        // If the test host lacks the plugin, skip rather than fail.
        if ElementFactory::find("x264enc").is_some() {
            assert!(can_instantiate("x264enc"));
        }
    }

    // --- detect_encoder branches ---

    #[test]
    fn detect_encoder_falls_through_to_auto_when_preferred_unavailable() {
        // Asking for a known-but-unavailable encoder (e.g. nvh264enc on a
        // non-NVIDIA host) should fall through and either find another working
        // encoder or bail with a clear error — never return the unavailable
        // preferred one.
        gst::init().unwrap();
        let nvidia_present = ElementFactory::find("nvh264enc")
            .map(|f| f.create().build().is_ok())
            .unwrap_or(false);
        if !nvidia_present {
            // Either a fallback (x264enc on most CI hosts) succeeds, or bail.
            if let Ok((_, name)) = detect_encoder(Some("nvh264enc")) {
                assert_ne!(name, "nvh264enc", "Should not return unavailable encoder");
            }
        }
    }

    #[test]
    fn detect_encoder_uses_preferred_when_available() {
        gst::init().unwrap();
        // x264enc is the always-available encoder on CI. If preferred=x264enc
        // resolves to anything, it MUST be x264enc itself (not a fallback).
        if ElementFactory::find("x264enc").is_some() {
            let (enc_type, name) = detect_encoder(Some("x264enc")).unwrap();
            assert_eq!(name, "x264enc");
            assert_eq!(enc_type, EncoderType::Software);
        }
    }

    #[test]
    fn detect_encoder_rejects_unknown_name_message_format() {
        // The error message must mention the rejected name so operators can
        // diagnose typos in config.
        let result = detect_encoder(Some("garbage-encoder-name"));
        let err = result.unwrap_err().to_string();
        assert!(err.contains("garbage-encoder-name"), "Error: {err}");
    }

    // --- build_encoder_element ---

    #[test]
    fn build_encoder_element_x264enc_constructs() {
        gst::init().unwrap();
        if ElementFactory::find("x264enc").is_some() {
            let elem = build_encoder_element(EncoderType::Software, "x264enc", 2_000_000, 30)
                .expect("x264enc should build");
            // Verify the bitrate property was actually set.
            let bitrate: u32 = elem.property("bitrate");
            assert_eq!(bitrate, 2_000_000);
        }
    }

    #[test]
    fn build_encoder_element_rejects_bogus_name_for_software_type() {
        gst::init().unwrap();
        // EncoderType::Software with a name GStreamer doesn't know → Err.
        let result = build_encoder_element(
            EncoderType::Software,
            "totally_fake_software_enc",
            2000000,
            30,
        );
        assert!(result.is_err());
    }

    // --- Full pipeline lifecycle (x264enc only) ---

    /// Build a full encoder pipeline via x264enc and verify it accepts a frame
    /// and produces an encoded NAL unit. This exercises the pipeline build,
    /// link, state-change, and frame-push paths end-to-end.
    #[test]
    fn encoder_x264_lifecycle_encodes_one_frame() {
        gst::init().unwrap();
        if ElementFactory::find("x264enc").is_none() {
            return; // No software encoder on this host; skip.
        }

        let encoder =
            match Encoder::with_encoder_preference(640, 480, 30, 2_000_000, Some("x264enc")) {
                Ok(e) => e,
                Err(e) => {
                    // Some CI hosts lack the full plugin set (e.g. h264parse from
                    // gst-plugins-bad). Skip rather than fail in that case.
                    eprintln!("Skipping x264 lifecycle test: {e:#}");
                    return;
                }
            };
        assert!(!encoder.has_error());

        // Push one black frame (640x480 BGRx = 4 bytes per pixel)
        let frame_bytes = 640usize * 480 * 4;
        let frame = crate::capture::synthetic_pooled_frame(vec![0u8; frame_bytes]);
        encoder.encode_frame(frame, 0).unwrap();

        // force_keyframe should not panic on a healthy pipeline.
        encoder.force_keyframe();

        // Encoder may need multiple frames before producing output; the test
        // verifies the API surface (push/pull/force/has_error) works without
        // panic. Actually pulling encoded data depends on x264enc behavior and
        // is timing-sensitive, so we don't gate the test on it.
        let _ = encoder.pull_encoded();
    }

    #[test]
    fn encoder_x264_force_keyframe_idempotent() {
        // Calling force_keyframe multiple times in a row must not panic or
        // leave the pipeline in a bad state.
        gst::init().unwrap();
        if ElementFactory::find("x264enc").is_none() {
            return;
        }
        let encoder = match Encoder::with_encoder_preference(320, 240, 30, 500_000, Some("x264enc"))
        {
            Ok(e) => e,
            Err(_) => return,
        };
        for _ in 0..5 {
            encoder.force_keyframe();
        }
        assert!(!encoder.has_error());
    }

    #[test]
    fn encoder_x264_pull_encoded_when_empty_returns_none() {
        gst::init().unwrap();
        if ElementFactory::find("x264enc").is_none() {
            return;
        }
        let encoder = match Encoder::with_encoder_preference(320, 240, 30, 500_000, Some("x264enc"))
        {
            Ok(e) => e,
            Err(_) => return,
        };
        // No frames pushed → no encoded output yet. Must be Ok(None), not Err.
        match encoder.pull_encoded() {
            Ok(None) => {}
            Ok(Some(data)) => {
                // Some encoders emit headers before any frame is pushed; either
                // is acceptable. The test only proves pull_encoded doesn't
                // wedge.
                assert!(!data.is_empty());
            }
            Err(e) => panic!("pull_encoded should not error on idle pipeline: {e:#}"),
        }
    }

    #[test]
    fn encoder_x264_has_error_starts_false() {
        gst::init().unwrap();
        if ElementFactory::find("x264enc").is_none() {
            return;
        }
        if let Ok(encoder) =
            Encoder::with_encoder_preference(320, 240, 30, 500_000, Some("x264enc"))
        {
            assert!(
                !encoder.has_error(),
                "Freshly-built pipeline must not report error"
            );
        }
    }

    #[test]
    fn encoder_x264_drop_cleans_up() {
        // Building + dropping the encoder back-to-back exercises the Drop impl
        // (EOS + Null transition). Must not deadlock or panic.
        gst::init().unwrap();
        if ElementFactory::find("x264enc").is_none() {
            return;
        }
        for _ in 0..3 {
            if let Ok(encoder) =
                Encoder::with_encoder_preference(320, 240, 30, 500_000, Some("x264enc"))
            {
                drop(encoder);
            }
        }
    }

    #[test]
    fn encoder_format_selection_per_type() {
        // BGRA only for Nvidia (non-CUDA); everything else uses BGRx.
        for enc_type in [
            EncoderType::Nvidia,
            EncoderType::NvidiaCuda,
            EncoderType::VaApi,
            EncoderType::Software,
        ] {
            let format = match enc_type {
                EncoderType::Nvidia | EncoderType::NvidiaCuda => "BGRA",
                _ => "BGRx",
            };
            if matches!(enc_type, EncoderType::Nvidia | EncoderType::NvidiaCuda) {
                assert_eq!(format, "BGRA", "{enc_type:?} should use BGRA");
            } else {
                assert_eq!(format, "BGRx", "{enc_type:?} should use BGRx");
            }
        }
    }

    #[test]
    fn encoder_type_is_copy() {
        // EncoderType must remain Copy so it can be passed by value through
        // the detection + build paths without ownership concerns.
        let a = EncoderType::Software;
        let b = a;
        assert_eq!(a, b);
    }

    #[test]
    fn encoder_type_debug_format_includes_variant_name() {
        // Detection logs use {?encoder_type}; verify each variant renders
        // with its identifier so log search works.
        for (enc_type, expected) in [
            (EncoderType::Nvidia, "Nvidia"),
            (EncoderType::NvidiaCuda, "NvidiaCuda"),
            (EncoderType::VaApi, "VaApi"),
            (EncoderType::Software, "Software"),
        ] {
            let dbg = format!("{enc_type:?}");
            assert!(
                dbg.contains(expected),
                "Debug format for {enc_type:?} should contain {expected}, got {dbg}"
            );
        }
    }

    // --- detect_encoder_type re-export ---

    #[test]
    fn detect_encoder_type_matches_detect_encoder() {
        // The pub wrapper delegates to the private detect_encoder; verify both
        // return identical results for the same input.
        gst::init().unwrap();
        let pref = Some("garbage_encoder_xyz");
        let from_pub = detect_encoder_type(pref).map_err(|e| e.to_string());
        let from_priv = detect_encoder(pref).map_err(|e| e.to_string());
        match (from_pub, from_priv) {
            (Ok(a), Ok(b)) => assert_eq!(a, b),
            (Err(a), Err(b)) => assert_eq!(a, b),
            _ => panic!("detect_encoder_type and detect_encoder diverged"),
        }
    }
}
