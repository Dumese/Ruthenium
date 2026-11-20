// 远程声音播放：Opus（WebCodecs AudioDecoder）→ WebAudio 调度。
//
// 设计：
// - 每个 Opus 包解出一个 AudioBuffer，按 `nextTime` 顺序排到 AudioContext；
// - 60ms 抖动缓冲；落后超过 0.5s 直接重置到当前时间（丢积压，保证实时）；
// - 包序号跳变时按 20ms/包插入静音（简单丢包隐藏，避免时间轴塌缩）；
// - 音量/静音在本地 GainNode 上生效；不修改服务端数据。
import type { AudioStats } from './screenBus';

export class AudioPlayer {
  private ctx: AudioContext | null = null;
  private decoder: AudioDecoder | null = null;
  private gain: GainNode | null = null;
  private nextTime = 0;
  private decoded = 0;
  private peak = 0;
  private lastSeq: number | null = null;
  private scheduled: AudioBufferSourceNode[] = [];
  private stopped = false;
  private volume = 0.8;
  private muted = false;

  constructor(
    private onStats: (s: AudioStats) => void,
    private onError: (detail: string) => void,
  ) {}

  async start(): Promise<boolean> {
    if (typeof AudioDecoder === 'undefined') return false;
    const cfg: AudioDecoderConfig = { codec: 'opus', sampleRate: 48000, numberOfChannels: 2 };
    const support = await AudioDecoder.isConfigSupported(cfg).catch(() => null);
    if (!support?.supported) return false;

    const ctx = new AudioContext({ sampleRate: 48000 });
    await ctx.resume().catch(() => { /* 用户手势内一般可用 */ });
    const gain = ctx.createGain();
    gain.gain.value = this.muted ? 0 : this.volume;
    gain.connect(ctx.destination);
    // 先登记，configure 抛错时 stop() 才能关闭 context。
    this.ctx = ctx;
    this.gain = gain;

    const decoder = new AudioDecoder({
      output: (data) => this.play(data),
      error: (e) => {
        this.onError(String(e?.message ?? e));
        this.stop();
      },
    });
    try {
      decoder.configure(cfg);
    } catch (e) {
      try { decoder.close(); } catch { /* ignore */ }
      try { void ctx.close(); } catch { /* ignore */ }
      this.ctx = null;
      this.gain = null;
      throw e;
    }

    this.decoder = decoder;
    this.nextTime = 0;
    this.lastSeq = null;
    this.stopped = false;
    return true;
  }

  /** 收到一个 Opus 包（base64）。 */
  push(base64: string, seq: number): void {
    if (this.stopped || !this.decoder || this.decoder.state !== 'configured') return;
    if (this.lastSeq != null && seq > this.lastSeq + 1) {
      // 丢包：按 20ms/包推进时间轴，后续包不会挤在一起
      this.nextTime += (seq - this.lastSeq - 1) * 0.02;
    }
    this.lastSeq = seq;
    let data: Uint8Array;
    try {
      data = Uint8Array.from(atob(base64), (c) => c.charCodeAt(0));
    } catch {
      return; // 非法 base64：丢弃该包，不打断后续音频
    }
    try {
      this.decoder.decode(new EncodedAudioChunk({
        type: 'key',
        timestamp: Math.round(performance.now() * 1000),
        data,
      }));
    } catch {
      /* 解码器忙/关闭：丢弃该包 */
    }
  }

  private play(data: AudioData): void {
    const ctx = this.ctx;
    const gain = this.gain;
    if (!ctx || !gain || this.stopped) {
      data.close();
      return;
    }
    const frames = data.numberOfFrames;
    const channels = Math.min(2, data.numberOfChannels);
    const buffer = ctx.createBuffer(channels, frames, data.sampleRate);
    const tmp = new Float32Array(frames);
    let peak = this.peak;
    for (let ch = 0; ch < channels; ch += 1) {
      try {
        data.copyTo(tmp, { planeIndex: ch, format: 'f32-planar' });
      } catch {
        try {
          data.copyTo(tmp, { planeIndex: ch });
        } catch {
          tmp.fill(0);
        }
      }
      buffer.copyToChannel(tmp, ch);
      for (let i = 0; i < frames; i += 1) {
        const v = Math.abs(tmp[i]);
        if (v > peak) peak = v;
      }
    }
    const sampleRate = data.sampleRate;
    data.close();

    const src = ctx.createBufferSource();
    src.buffer = buffer;
    src.connect(gain);
    const now = ctx.currentTime;
    let start = Math.max(now + 0.06, this.nextTime);
    if (start > now + 0.5) {
      // 积压过多：必须停掉已排期的旧 source，否则新包会和旧音频叠在一起。
      for (const old of this.scheduled) {
        try { old.stop(); } catch { /* 已结束 */ }
      }
      this.scheduled = [];
      this.nextTime = 0;
      start = now + 0.06;
    }
    src.start(start);
    this.scheduled.push(src);
    src.onended = () => {
      this.scheduled = this.scheduled.filter((x) => x !== src);
    };
    this.nextTime = start + buffer.duration;

    this.decoded += 1;
    this.peak = peak;
    if (this.decoded % 5 === 0) {
      this.onStats({
        decoded: this.decoded,
        peak: this.peak,
        rate: sampleRate,
        channels,
      });
    }
  }

  setVolume(v: number): void {
    this.volume = Math.min(1, Math.max(0, v));
    if (this.gain && !this.muted) this.gain.gain.value = this.volume;
  }

  setMuted(m: boolean): void {
    this.muted = m;
    if (this.gain) this.gain.gain.value = m ? 0 : this.volume;
  }

  stop(): void {
    this.stopped = true;
    for (const src of this.scheduled) {
      try { src.stop(); } catch { /* 已结束 */ }
    }
    this.scheduled = [];
    try {
      this.decoder?.close();
    } catch { /* 已关闭 */ }
    this.decoder = null;
    try {
      void this.ctx?.close();
    } catch { /* 已关闭 */ }
    this.ctx = null;
    this.gain = null;
    this.nextTime = 0;
    this.lastSeq = null;
  }
}
