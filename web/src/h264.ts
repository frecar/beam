/**
 * H.264 Annex B utilities for extracting codec information from NAL units.
 */

/**
 * Extract avc1 codec string from SPS in Annex B keyframe data.
 * Scans for SPS NAL (type 7), reads profile_idc, constraint_flags, level_idc.
 * Returns e.g. "avc1.4d0033" or null if no SPS found.
 */
export function extractCodecFromAnnexB(payload: Uint8Array): string | null {
  for (let i = 0; i + 4 < payload.length; i++) {
    let nalStart = -1;
    if (payload[i] === 0 && payload[i + 1] === 0 && payload[i + 2] === 0 && payload[i + 3] === 1) {
      nalStart = i + 4;
    } else if (payload[i] === 0 && payload[i + 1] === 0 && payload[i + 2] === 1) {
      nalStart = i + 3;
    }
    if (nalStart >= 0 && nalStart + 3 < payload.length) {
      const nalType = payload[nalStart] & 0x1f;
      if (nalType === 7) {
        // SPS found
        const profile = payload[nalStart + 1];
        const compat = payload[nalStart + 2];
        const level = payload[nalStart + 3];
        return `avc1.${profile.toString(16).padStart(2, '0')}${compat.toString(16).padStart(2, '0')}${level.toString(16).padStart(2, '0')}`;
      }
    }
  }
  return null;
}
