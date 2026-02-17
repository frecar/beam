//! Binary video/audio frame header for WebSocket transport.
//!
//! 24 bytes, little-endian:
//! ```text
//! [0..4]   magic: 0x42454156 ("BEAV")
//! [4]      version: 1
//! [5]      flags: bit 0 = keyframe, bit 1 = audio
//! [6..8]   width (u16)
//! [8..10]  height (u16)
//! [10..12] reserved (u16, must be 0)
//! [12..20] timestamp_us (u64) — microseconds since capture start
//! [20..24] payload_length (u32)
//! [24..]   payload (H.264 Annex B for video, Opus for audio)
//! ```

pub const FRAME_HEADER_SIZE: usize = 24;
pub const FRAME_MAGIC: u32 = 0x5641_4542; // "BEAV" in LE
pub const FRAME_VERSION: u8 = 1;

pub const FLAG_KEYFRAME: u8 = 0x01;
pub const FLAG_AUDIO: u8 = 0x02;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrameHeader {
    pub flags: u8,
    pub width: u16,
    pub height: u16,
    pub timestamp_us: u64,
    pub payload_length: u32,
}

impl VideoFrameHeader {
    /// Create a new video frame header.
    pub fn video(width: u16, height: u16, timestamp_us: u64, payload_length: u32, keyframe: bool) -> Self {
        Self {
            flags: if keyframe { FLAG_KEYFRAME } else { 0 },
            width,
            height,
            timestamp_us,
            payload_length,
        }
    }

    /// Create a new audio frame header.
    pub fn audio(timestamp_us: u64, payload_length: u32) -> Self {
        Self {
            flags: FLAG_AUDIO,
            width: 0,
            height: 0,
            timestamp_us,
            payload_length,
        }
    }

    pub fn is_keyframe(&self) -> bool {
        self.flags & FLAG_KEYFRAME != 0
    }

    pub fn is_audio(&self) -> bool {
        self.flags & FLAG_AUDIO != 0
    }

    /// Serialize header to 24-byte little-endian buffer.
    pub fn serialize(&self, buf: &mut [u8; FRAME_HEADER_SIZE]) {
        buf[0..4].copy_from_slice(&FRAME_MAGIC.to_le_bytes());
        buf[4] = FRAME_VERSION;
        buf[5] = self.flags;
        buf[6..8].copy_from_slice(&self.width.to_le_bytes());
        buf[8..10].copy_from_slice(&self.height.to_le_bytes());
        buf[10..12].copy_from_slice(&0u16.to_le_bytes()); // reserved
        buf[12..20].copy_from_slice(&self.timestamp_us.to_le_bytes());
        buf[20..24].copy_from_slice(&self.payload_length.to_le_bytes());
    }

    /// Serialize header + payload into a single Vec.
    pub fn serialize_with_payload(&self, payload: &[u8]) -> Vec<u8> {
        let mut buf = vec![0u8; FRAME_HEADER_SIZE + payload.len()];
        let mut header_buf = [0u8; FRAME_HEADER_SIZE];
        self.serialize(&mut header_buf);
        buf[..FRAME_HEADER_SIZE].copy_from_slice(&header_buf);
        buf[FRAME_HEADER_SIZE..].copy_from_slice(payload);
        buf
    }

    /// Deserialize header from a byte slice (must be at least 24 bytes).
    pub fn deserialize(buf: &[u8]) -> Result<Self, FrameError> {
        if buf.len() < FRAME_HEADER_SIZE {
            return Err(FrameError::TooShort(buf.len()));
        }

        let magic = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if magic != FRAME_MAGIC {
            return Err(FrameError::BadMagic(magic));
        }

        let version = buf[4];
        if version != FRAME_VERSION {
            return Err(FrameError::UnsupportedVersion(version));
        }

        Ok(Self {
            flags: buf[5],
            width: u16::from_le_bytes([buf[6], buf[7]]),
            height: u16::from_le_bytes([buf[8], buf[9]]),
            timestamp_us: u64::from_le_bytes([
                buf[12], buf[13], buf[14], buf[15], buf[16], buf[17], buf[18], buf[19],
            ]),
            payload_length: u32::from_le_bytes([buf[20], buf[21], buf[22], buf[23]]),
        })
    }

    /// Validate that the buffer contains a complete frame (header + payload).
    pub fn validate_complete(buf: &[u8]) -> Result<(), FrameError> {
        let header = Self::deserialize(buf)?;
        let expected = FRAME_HEADER_SIZE + header.payload_length as usize;
        if buf.len() < expected {
            return Err(FrameError::IncompletePayload {
                expected: header.payload_length as usize,
                actual: buf.len() - FRAME_HEADER_SIZE,
            });
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("buffer too short: {0} bytes (need at least {FRAME_HEADER_SIZE})")]
    TooShort(usize),
    #[error("bad magic: 0x{0:08x} (expected 0x{FRAME_MAGIC:08x})")]
    BadMagic(u32),
    #[error("unsupported version: {0} (expected {FRAME_VERSION})")]
    UnsupportedVersion(u8),
    #[error("incomplete payload: expected {expected} bytes, got {actual}")]
    IncompletePayload { expected: usize, actual: usize },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_header_roundtrip() {
        let header = VideoFrameHeader::video(1920, 1080, 123456, 65536, true);
        let mut buf = [0u8; FRAME_HEADER_SIZE];
        header.serialize(&mut buf);
        let parsed = VideoFrameHeader::deserialize(&buf).unwrap();
        assert_eq!(header, parsed);
        assert!(parsed.is_keyframe());
        assert!(!parsed.is_audio());
    }

    #[test]
    fn audio_header_roundtrip() {
        let header = VideoFrameHeader::audio(999999, 480);
        let mut buf = [0u8; FRAME_HEADER_SIZE];
        header.serialize(&mut buf);
        let parsed = VideoFrameHeader::deserialize(&buf).unwrap();
        assert_eq!(header, parsed);
        assert!(!parsed.is_keyframe());
        assert!(parsed.is_audio());
    }

    #[test]
    fn p_frame_no_keyframe_flag() {
        let header = VideoFrameHeader::video(1920, 1080, 0, 1024, false);
        assert!(!header.is_keyframe());
        assert!(!header.is_audio());
        assert_eq!(header.flags, 0);
    }

    #[test]
    fn serialize_with_payload() {
        let payload = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let header = VideoFrameHeader::video(640, 480, 42, 4, true);
        let buf = header.serialize_with_payload(&payload);
        assert_eq!(buf.len(), FRAME_HEADER_SIZE + 4);
        // Verify header
        let parsed = VideoFrameHeader::deserialize(&buf).unwrap();
        assert_eq!(parsed.width, 640);
        assert_eq!(parsed.height, 480);
        assert_eq!(parsed.timestamp_us, 42);
        assert_eq!(parsed.payload_length, 4);
        // Verify payload
        assert_eq!(&buf[FRAME_HEADER_SIZE..], &payload);
    }

    #[test]
    fn deserialize_too_short() {
        let buf = [0u8; 10];
        match VideoFrameHeader::deserialize(&buf) {
            Err(FrameError::TooShort(10)) => {}
            other => panic!("expected TooShort(10), got {:?}", other),
        }
    }

    #[test]
    fn deserialize_bad_magic() {
        let mut buf = [0u8; FRAME_HEADER_SIZE];
        buf[0..4].copy_from_slice(&0xDEADBEEFu32.to_le_bytes());
        match VideoFrameHeader::deserialize(&buf) {
            Err(FrameError::BadMagic(0xDEADBEEF)) => {}
            other => panic!("expected BadMagic, got {:?}", other),
        }
    }

    #[test]
    fn deserialize_bad_version() {
        let mut buf = [0u8; FRAME_HEADER_SIZE];
        buf[0..4].copy_from_slice(&FRAME_MAGIC.to_le_bytes());
        buf[4] = 99;
        match VideoFrameHeader::deserialize(&buf) {
            Err(FrameError::UnsupportedVersion(99)) => {}
            other => panic!("expected UnsupportedVersion(99), got {:?}", other),
        }
    }

    #[test]
    fn validate_complete_ok() {
        let payload = vec![0u8; 100];
        let header = VideoFrameHeader::video(1920, 1080, 0, 100, false);
        let buf = header.serialize_with_payload(&payload);
        assert!(VideoFrameHeader::validate_complete(&buf).is_ok());
    }

    #[test]
    fn validate_complete_incomplete_payload() {
        let payload = vec![0u8; 50];
        let header = VideoFrameHeader::video(1920, 1080, 0, 100, false);
        let buf = header.serialize_with_payload(&payload);
        // Truncate — header says 100 bytes but only 50 present
        match VideoFrameHeader::validate_complete(&buf) {
            Err(FrameError::IncompletePayload {
                expected: 100,
                actual: 50,
            }) => {}
            other => panic!("expected IncompletePayload, got {:?}", other),
        }
    }

    #[test]
    fn magic_bytes_spell_beav() {
        let bytes = FRAME_MAGIC.to_le_bytes();
        assert_eq!(&bytes, b"BEAV");
    }

    #[test]
    fn header_size_is_24() {
        assert_eq!(FRAME_HEADER_SIZE, 24);
    }
}
