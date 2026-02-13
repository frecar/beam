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
    pub fn new(sample_rate: u32, channels: u16) -> anyhow::Result<Self> {
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
            None,                          // Default server
            "beam-agent",                  // Application name
            pulse::stream::Direction::Record,
            Some("@DEFAULT_MONITOR@"),     // Capture system audio output
            "audio-capture",               // Stream description
            &spec,
            None,                          // Default channel map
            Some(&buf_attr),               // Low-latency buffer (20ms fragments)
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
