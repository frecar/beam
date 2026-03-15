import { describe, it, expect } from "vitest";
import { extractCodecFromAnnexB } from "./h264";

describe("extractCodecFromAnnexB", () => {
  it("extracts Main profile codec string from 4-byte start code", () => {
    // SPS NAL: type 7, profile_idc=0x4d (Main), constraint=0x00, level=0x33 (5.1)
    const data = new Uint8Array([
      0x00, 0x00, 0x00, 0x01, 0x67, 0x4d, 0x00, 0x33, 0xac, 0xd9,
    ]);
    expect(extractCodecFromAnnexB(data)).toBe("avc1.4d0033");
  });

  it("extracts High profile codec string", () => {
    // SPS NAL: type 7, profile_idc=0x64 (High), constraint=0x00, level=0x33
    const data = new Uint8Array([
      0x00, 0x00, 0x00, 0x01, 0x67, 0x64, 0x00, 0x33, 0xac, 0xd9,
    ]);
    expect(extractCodecFromAnnexB(data)).toBe("avc1.640033");
  });

  it("extracts codec from SPS after PPS in access unit", () => {
    // PPS (type 8) + SPS (type 7) with Main profile
    const data = new Uint8Array([
      0x00, 0x00, 0x00, 0x01, 0x68, 0xee, 0x3c, 0x80, // PPS
      0x00, 0x00, 0x00, 0x01, 0x67, 0x4d, 0x40, 0x28, // SPS: Main, constraint_set1, level 4.0
      0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x80, 0x40, // IDR
    ]);
    expect(extractCodecFromAnnexB(data)).toBe("avc1.4d4028");
  });

  it("extracts codec from 3-byte start code", () => {
    const data = new Uint8Array([
      0x00, 0x00, 0x01, 0x67, 0x4d, 0x00, 0x28, 0xac, 0xd9,
    ]);
    expect(extractCodecFromAnnexB(data)).toBe("avc1.4d0028");
  });

  it("returns null when no SPS present", () => {
    // Only IDR (type 5), no SPS
    const data = new Uint8Array([
      0x00, 0x00, 0x00, 0x01, 0x65, 0x88, 0x80, 0x40,
    ]);
    expect(extractCodecFromAnnexB(data)).toBeNull();
  });

  it("returns null for empty data", () => {
    expect(extractCodecFromAnnexB(new Uint8Array([]))).toBeNull();
  });

  it("returns null for data too short for SPS", () => {
    // Start code + SPS type but not enough bytes for profile/level
    const data = new Uint8Array([0x00, 0x00, 0x00, 0x01, 0x67, 0x4d]);
    expect(extractCodecFromAnnexB(data)).toBeNull();
  });

  it("handles real nvh264enc Main profile SPS", () => {
    // Real SPS from nvh264enc: 0x67 0x4d 0x00 0x28 ...
    const data = new Uint8Array([
      0x00, 0x00, 0x00, 0x01,
      0x67, 0x4d, 0x00, 0x28, 0xac, 0xd9, 0x40, 0x78,
      0x02, 0x27, 0xe5, 0xc0, 0x44, 0x00, 0x00, 0x03,
    ]);
    expect(extractCodecFromAnnexB(data)).toBe("avc1.4d0028");
  });
});
