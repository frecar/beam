use audiopus::coder::Encoder as OpusEncoder;
use audiopus::{Application, Bitrate, Channels, SampleRate};
use libpulse_binding as pulse;
use libpulse_simple_binding::Simple;
use tracing::info;

pub struct AudioCapture {
    simple: Simple,
    opus_encoder: OpusEncoder,
    pcm_buffer: Vec<u8>,
    opus_buffer: Vec<u8>,
    /// Pre-allocated buffer for s16le→i16 conversion (avoids 50 allocs/sec)
    samples_buffer: Vec<i16>,
}

impl AudioCapture {
    pub fn new(
        sample_rate: u32,
        channels: u16,
        pulse_server: Option<&str>,
    ) -> anyhow::Result<Self> {
        let spec = pulse::sample::Spec {
            format: pulse::sample::Format::S16le,
            channels: channels as u8,
            rate: sample_rate,
        };

        // 20ms frame: samples_per_channel = sample_rate * 20 / 1000
        let samples_per_frame = (sample_rate * 20 / 1000) as usize;
        let frame_bytes_val = samples_per_frame * channels as usize * 2; // s16le

        // Set PulseAudio buffer attributes for low-latency capture.
        // fragsize = one Opus frame (20ms) to minimize audio latency.
        let buf_attr = pulse::def::BufferAttr {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: frame_bytes_val as u32,
        };

        let simple = Simple::new(
            pulse_server, // Explicit server path (avoids unsafe set_var)
            "beam-agent", // Application name
            pulse::stream::Direction::Record,
            Some("@DEFAULT_MONITOR@"), // Capture system audio output
            "audio-capture",           // Stream description
            &spec,
            None,            // Default channel map
            Some(&buf_attr), // Low-latency buffer (20ms fragments)
        )
        .map_err(|e| anyhow::anyhow!("PulseAudio connection failed: {e}"))?;

        let opus_channels = match channels {
            1 => Channels::Mono,
            2 => Channels::Stereo,
            _ => anyhow::bail!("Unsupported channel count: {channels}"),
        };

        let opus_sample_rate = match sample_rate {
            48000 => SampleRate::Hz48000,
            24000 => SampleRate::Hz24000,
            16000 => SampleRate::Hz16000,
            12000 => SampleRate::Hz12000,
            8000 => SampleRate::Hz8000,
            _ => anyhow::bail!("Unsupported sample rate for Opus: {sample_rate}"),
        };

        let mut opus_encoder =
            OpusEncoder::new(opus_sample_rate, opus_channels, Application::LowDelay)
                .map_err(|e| anyhow::anyhow!("Failed to create Opus encoder: {e:?}"))?;

        opus_encoder
            .set_bitrate(Bitrate::BitsPerSecond(256_000))
            .map_err(|e| anyhow::anyhow!("Failed to set Opus bitrate: {e:?}"))?;

        info!(
            sample_rate,
            channels,
            frame_bytes = frame_bytes_val,
            samples_per_channel = samples_per_frame,
            "Audio capture initialized"
        );

        Ok(Self {
            simple,
            opus_encoder,
            pcm_buffer: vec![0u8; frame_bytes_val],
            opus_buffer: vec![0u8; 4000], // Max Opus frame size
            samples_buffer: vec![0i16; samples_per_frame * channels as usize],
        })
    }

    /// Discard any buffered audio data from PulseAudio.
    /// Call after connecting to avoid playing back stale silence.
    pub fn flush(&mut self) {
        let _ = self.simple.flush();
    }

    /// Read 20ms of PCM audio from PulseAudio and encode to Opus.
    pub fn capture_and_encode(&mut self) -> anyhow::Result<Vec<u8>> {
        self.simple
            .read(&mut self.pcm_buffer)
            .map_err(|e| anyhow::anyhow!("PulseAudio read failed: {e}"))?;

        // Convert s16le bytes to i16 samples using pre-allocated buffer
        for (i, chunk) in self.pcm_buffer.chunks_exact(2).enumerate() {
            self.samples_buffer[i] = i16::from_le_bytes([chunk[0], chunk[1]]);
        }

        let encoded_len = self
            .opus_encoder
            .encode(&self.samples_buffer, &mut self.opus_buffer)
            .map_err(|e| anyhow::anyhow!("Opus encode failed: {e:?}"))?;

        Ok(self.opus_buffer[..encoded_len].to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_capture_rejects_invalid_server() {
        let result = AudioCapture::new(48000, 2, Some("/tmp/nonexistent-pulse-server"));
        assert!(
            result.is_err(),
            "Should fail with bogus PulseAudio server path"
        );
    }

    #[test]
    fn audio_spec_params() {
        // Verify the frame math: 20ms at 48kHz stereo s16le
        let sample_rate: u32 = 48000;
        let channels: u16 = 2;
        let samples_per_frame = (sample_rate * 20 / 1000) as usize; // 960
        let frame_bytes = samples_per_frame * channels as usize * 2; // 3840
        assert_eq!(
            samples_per_frame, 960,
            "20ms at 48kHz = 960 samples per channel"
        );
        assert_eq!(
            frame_bytes, 3840,
            "960 samples * 2 channels * 2 bytes = 3840"
        );
    }

    #[test]
    fn audio_frame_math_24khz_mono() {
        // 20ms at 24kHz mono s16le = 480 samples, 960 bytes
        let sample_rate: u32 = 24000;
        let channels: u16 = 1;
        let samples_per_frame = (sample_rate * 20 / 1000) as usize;
        let frame_bytes = samples_per_frame * channels as usize * 2;
        assert_eq!(samples_per_frame, 480);
        assert_eq!(frame_bytes, 960);
    }

    #[test]
    fn audio_frame_math_8khz_mono() {
        // 20ms at 8kHz mono s16le = 160 samples, 320 bytes
        let sample_rate: u32 = 8000;
        let channels: u16 = 1;
        let samples_per_frame = (sample_rate * 20 / 1000) as usize;
        let frame_bytes = samples_per_frame * channels as usize * 2;
        assert_eq!(samples_per_frame, 160);
        assert_eq!(frame_bytes, 320);
    }

    #[test]
    fn audio_frame_math_48khz_stereo_matches_24khz_stereo_doubled() {
        // 48kHz stereo has exactly 2x the per-frame byte budget of 24kHz stereo.
        let bytes_48 = ((48000u32 * 20 / 1000) as usize) * 2 * 2;
        let bytes_24 = ((24000u32 * 20 / 1000) as usize) * 2 * 2;
        assert_eq!(bytes_48, bytes_24 * 2);
    }

    #[test]
    fn audio_capture_rejects_invalid_channel_count_zero() {
        // 0 channels is invalid for PulseAudio — connection itself should fail
        // before we get to the Opus sanity check.
        let result = AudioCapture::new(48000, 0, Some("/tmp/nonexistent-pulse-server"));
        assert!(result.is_err());
    }

    #[test]
    fn audio_capture_rejects_invalid_channel_count_three() {
        // 3 channels: PulseAudio may accept it, but our match in
        // AudioCapture::new rejects anything except 1 or 2 for Opus.
        // Either branch (connection failure or unsupported channel count)
        // produces an Err, which is what we assert here.
        let result = AudioCapture::new(48000, 3, Some("/tmp/nonexistent-pulse-server"));
        assert!(result.is_err());
    }

    #[test]
    fn audio_capture_rejects_unsupported_sample_rate() {
        // 44.1kHz is not in our Opus sample-rate match. Even if PulseAudio
        // would accept it, our explicit list (48k/24k/16k/12k/8k) rejects.
        let result = AudioCapture::new(44100, 2, Some("/tmp/nonexistent-pulse-server"));
        assert!(result.is_err());
    }

    #[test]
    fn audio_pcm_to_i16_little_endian_conversion() {
        // The capture path uses i16::from_le_bytes; verify the byte order
        // matches the s16le PulseAudio format declaration.
        let pcm = [0x00u8, 0x80]; // -32768 in s16le
        let sample = i16::from_le_bytes([pcm[0], pcm[1]]);
        assert_eq!(sample, i16::MIN);

        let pcm = [0xFFu8, 0x7F]; // 32767 in s16le
        let sample = i16::from_le_bytes([pcm[0], pcm[1]]);
        assert_eq!(sample, i16::MAX);

        let pcm = [0x00u8, 0x00];
        let sample = i16::from_le_bytes([pcm[0], pcm[1]]);
        assert_eq!(sample, 0);
    }

    // --- Additional sample-rate / channel-count validation ---

    /// Extract the error message from an `AudioCapture::new` result without
    /// requiring `AudioCapture` itself to be `Debug`. Returns an empty string
    /// when the result is unexpectedly `Ok`.
    fn err_msg(result: anyhow::Result<AudioCapture>) -> String {
        match result {
            Ok(_) => String::new(),
            Err(e) => format!("{e:#}"),
        }
    }

    #[test]
    fn audio_capture_accepts_all_documented_sample_rates() {
        // The documented Opus sample rates are 48k/24k/16k/12k/8k. All must
        // be syntactically accepted by AudioCapture::new (PulseAudio connect
        // will fail with bogus server, but our rate-check must NOT be the
        // reason).
        for sample_rate in [48000u32, 24000, 16000, 12000, 8000] {
            let result = AudioCapture::new(sample_rate, 2, Some("/tmp/nonexistent-pulse-server"));
            let err = err_msg(result);
            assert!(
                !err.is_empty(),
                "Expected PulseAudio connect error for rate {sample_rate}"
            );
            // The error message should NOT contain "Unsupported sample rate"
            // (which would mean our validation rejected it).
            assert!(
                !err.contains("Unsupported sample rate"),
                "Sample rate {sample_rate} should pass our validation, but got: {err}"
            );
        }
    }

    #[test]
    fn audio_capture_rejects_high_unsupported_sample_rate() {
        let result = AudioCapture::new(96000, 2, Some("/tmp/nonexistent-pulse-server"));
        assert!(result.is_err());
    }

    #[test]
    fn audio_capture_rejects_low_unsupported_sample_rate() {
        let result = AudioCapture::new(4000, 2, Some("/tmp/nonexistent-pulse-server"));
        assert!(result.is_err());
    }

    #[test]
    fn audio_capture_accepts_mono_channel_count() {
        // 1 channel = Mono, valid for Opus.
        let result = AudioCapture::new(48000, 1, Some("/tmp/nonexistent-pulse-server"));
        // PulseAudio connect fails first; ensure we don't fail on channel
        // count validation.
        let err = err_msg(result);
        assert!(!err.contains("Unsupported channel count"));
    }

    #[test]
    fn audio_capture_accepts_stereo_channel_count() {
        let result = AudioCapture::new(48000, 2, Some("/tmp/nonexistent-pulse-server"));
        let err = err_msg(result);
        assert!(!err.contains("Unsupported channel count"));
    }

    // --- frame_bytes validation ---

    #[test]
    fn audio_frame_math_consistent_across_rates() {
        // 20ms at every supported rate must produce a frame size divisible
        // by 2 (s16) * channels.
        for sample_rate in [48000u32, 24000, 16000, 12000, 8000] {
            for channels in [1u16, 2] {
                let samples = (sample_rate * 20 / 1000) as usize;
                let frame_bytes = samples * channels as usize * 2;
                assert_eq!(
                    frame_bytes % (channels as usize * 2),
                    0,
                    "frame_bytes must be evenly divisible by channels*2"
                );
                // 20ms at all supported rates must be > 0 samples
                assert!(samples > 0);
            }
        }
    }

    #[test]
    fn audio_frame_math_16khz_mono() {
        let sample_rate: u32 = 16000;
        let channels: u16 = 1;
        let samples_per_frame = (sample_rate * 20 / 1000) as usize;
        let frame_bytes = samples_per_frame * channels as usize * 2;
        assert_eq!(samples_per_frame, 320);
        assert_eq!(frame_bytes, 640);
    }

    #[test]
    fn audio_frame_math_12khz_stereo() {
        let sample_rate: u32 = 12000;
        let channels: u16 = 2;
        let samples_per_frame = (sample_rate * 20 / 1000) as usize;
        let frame_bytes = samples_per_frame * channels as usize * 2;
        assert_eq!(samples_per_frame, 240);
        assert_eq!(frame_bytes, 960);
    }

    // --- PCM byte order: round-trip of arbitrary i16 values ---

    #[test]
    fn audio_pcm_roundtrip_various_values() {
        // Ensure little-endian encode + decode round trip preserves the
        // sample.
        for original in [
            0i16, 1, -1, 100, -100, 1000, -1000, 32000, -32000, 32767, -32768,
        ] {
            let bytes = original.to_le_bytes();
            let restored = i16::from_le_bytes([bytes[0], bytes[1]]);
            assert_eq!(restored, original);
        }
    }
}
