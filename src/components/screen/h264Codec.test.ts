import { describe, expect, it } from 'vitest';
import { codecStringFromKeyframe } from './h264Codec';

function annexb(...nals: number[][]): Uint8Array {
  const out: number[] = [];
  for (const nal of nals) out.push(0, 0, 0, 1, ...nal);
  return Uint8Array.from(out);
}

describe('codecStringFromKeyframe', () => {
  it('reads profile/constraints/level from SPS', () => {
    // High 4.0：profile_idc=100(0x64) constraint=0x00 level=40(0x28)
    const sps = annexb([0x67, 0x64, 0x00, 0x28, 0xac]);
    expect(codecStringFromKeyframe(sps)).toBe('avc1.640028');
  });

  it('supports 3-byte start codes', () => {
    const bytes = Uint8Array.from([0, 0, 1, 0x67, 0x42, 0xe0, 0x1e, 0x00]);
    expect(codecStringFromKeyframe(bytes)).toBe('avc1.42E01E');
  });

  it('finds SPS after PPS and ignores AUD', () => {
    const pps = [0x68, 0xce, 0x3c, 0x80];
    const sps = [0x67, 0x4d, 0x40, 0x1f, 0x00];
    const idr = [0x65, 0x88];
    expect(codecStringFromKeyframe(annexb([0x09, 0xf0], pps, sps, idr))).toBe('avc1.4D401F');
  });

  it('returns null when the SPS is missing', () => {
    expect(codecStringFromKeyframe(annexb([0x65, 0x88, 0x84]))).toBeNull();
    expect(codecStringFromKeyframe(Uint8Array.from([]))).toBeNull();
  });
});
