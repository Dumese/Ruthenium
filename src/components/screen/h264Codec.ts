// H.264 关键帧的 SPS → WebCodecs codec string（`avc1.PPCCLL`）。
//
// 硬编码 `avc1.42E01E`（Baseline 3.0）与 1080p 实际码流（常见 High 4.0）不符时，
// 个别硬解/驱动会拒绝配置或花屏，且不一定会触发回退。SPS 去掉 NAL 头后的前
// 3 个字节就是 profile_idc / constraint flags / level_idc，不需要完整解析。

function hex2(n: number): string {
  return n.toString(16).padStart(2, '0').toUpperCase();
}

/** 从 Annex B 关键帧中找 SPS（NAL type 7），返回 `avc1.PPCCLL`；找不到返回 null。 */
export function codecStringFromKeyframe(data: Uint8Array): string | null {
  for (let i = 0; i + 4 < data.length; i++) {
    if (data[i] !== 0 || data[i + 1] !== 0) continue;
    let sc = 0;
    if (data[i + 2] === 1) sc = 3;
    else if (data[i + 2] === 0 && data[i + 3] === 1) sc = 4;
    else continue;
    const hdr = i + sc;
    if (hdr + 3 >= data.length) return null;
    const type = data[hdr] & 0x1f;
    if (type === 7) {
      return `avc1.${hex2(data[hdr + 1])}${hex2(data[hdr + 2])}${hex2(data[hdr + 3])}`;
    }
    if (type === 5) return null; // SPS 在 IDR 之后/缺失：交给服务端补发
  }
  return null;
}
