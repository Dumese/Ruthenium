// 远程桌面：canvas 只绘最新帧；工具栏画质/显示器/缩放/全屏；允许操作时进入键鼠控制。
// 操作模式优先 Pointer Lock（相对移动 + 本地光标圆点），未锁定时用绝对坐标。
import { useCallback, useEffect, useRef, useState } from 'react';
import { useAppStore } from '../../store/useAppStore';
import { T, useLang } from '../../i18n';
import * as api from '../../ipc/commands';
import { dropScreen, screenState, subscribeScreen, takeH264Frames } from './screenBus';
import type { AudioStats } from './screenBus';
import { AudioPlayer } from './audioPlayer';
import { Icon } from '../ui/Icon';

const QUALITIES = [45, 65, 80] as const;

/** 采集健康状态 → 提示文案；`live` 与未知状态不提示。 */
function captureWarnText(state: string): string | null {
  switch (state) {
    case 'secureDesktop':
      return T('rd.captureState.secureDesktop');
    case 'protectedWindow':
      return T('rd.captureState.protectedWindow');
    case 'exclusiveFullscreen':
      return T('rd.captureState.exclusiveFullscreen');
    case 'suspectedBlackout':
      return T('rd.captureState.suspectedBlackout');
    case 'retrying':
      return T('rd.captureState.retrying');
    default:
      return null;
  }
}

function qualityKey(q: number): string {
  return q <= 45 ? 'rd.quality.flow' : q >= 80 ? 'rd.quality.hd' : 'rd.quality.balanced';
}

/** 浏览器 KeyboardEvent.code → Windows 虚拟键码（可打印字符走 Unicode 注入）。 */
function vkOf(code: string): number | null {
  if (/^Key[A-Z]$/.test(code)) return code.charCodeAt(3);
  if (/^Digit[0-9]$/.test(code)) return 48 + Number(code.slice(5));
  if (/^F([1-9]|1[0-2])$/.test(code)) return 111 + Number(code.slice(1));
  const map: Record<string, number> = {
    Enter: 13, Escape: 27, Backspace: 8, Tab: 9, Space: 32, Delete: 46, Insert: 45,
    ArrowLeft: 37, ArrowUp: 38, ArrowRight: 39, ArrowDown: 40,
    Home: 36, End: 35, PageUp: 33, PageDown: 34,
    ShiftLeft: 16, ShiftRight: 16, ControlLeft: 17, ControlRight: 17,
    AltLeft: 18, AltRight: 18, MetaLeft: 91, MetaRight: 92,
    Minus: 189, Equal: 187, BracketLeft: 219, BracketRight: 221, Semicolon: 186,
    Quote: 222, Backquote: 192, Comma: 188, Period: 190, Slash: 191, Backslash: 220,
  };
  return map[code] ?? null;
}

const clamp01 = (v: number) => Math.min(1, Math.max(0, v));

import { codecStringFromKeyframe } from './h264Codec';

/** WebCodecs：H.264 Annex B（SPS/PPS 在码流内）；不支持时保持 JPEG。 */
const DECODER_CONFIG = {
  codec: 'avc1.42E01E',
  optimizeForLatency: true,
  avc: { format: 'annexb' },
} as unknown as VideoDecoderConfig;

/** 常见 H.264 配置（协商阶段探测 WebCodecs 能力用）。 */
const DECODER_PROBES = ['avc1.640028', 'avc1.64001F', 'avc1.4D401F', 'avc1.42E01E'];

const qualityKeyOf = (peerId: string) => `ruthenium.rd.quality.${peerId}`;

export function RemoteView() {
  useLang();
  const active = useAppStore((s) => s.active);
  const sessions = useAppStore((s) => s.sessions);
  const closeSession = useAppStore((s) => s.closeSession);
  const showToast = useAppStore((s) => s.showToast);
  const rtt = useAppStore((s) => (s.active ? s.rtt[s.active] : undefined));
  const s = sessions.find((x) => x.id === active);
  const sessionId = s && s.kind === 'screen' ? s.id : null;
  const st = sessionId ? screenState(sessionId) : undefined;
  const inputEndedId = useAppStore((x) => (sessionId ? x.inputEnded[sessionId] : undefined));
  const clearInputEnded = useAppStore((x) => x.clearInputEnded);
  const clipEndedId = useAppStore((x) => (sessionId ? x.clipboardEnded[sessionId] : undefined));
  const clearClipboardEnded = useAppStore((x) => x.clearClipboardEnded);
  const [tick, setTick] = useState(0);
  const [quality, setQuality] = useState<number>(65);
  const [monitor, setMonitor] = useState<number>(0);
  const [controlling, setControlling] = useState(false);
  const [requestingInput, setRequestingInput] = useState(false);
  const inputUnsupportedWarned = useRef(false);
  const [locked, setLocked] = useState(false);
  const [cursor, setCursor] = useState<{ x: number; y: number } | null>(null);
  const [zoom, setZoom] = useState<'fit' | 'original'>('fit');
  const [clipId, setClipId] = useState<string | null>(null);
  const [clipBusy, setClipBusy] = useState(false);
  const [audioOn, setAudioOn] = useState(false);
  const [micOn, setMicOn] = useState(false);
  const [audioStats, setAudioStats] = useState<AudioStats>({ decoded: 0, peak: 0, rate: 48000, channels: 2 });
  const [volume, setVolume] = useState(0.8);
  const [muted, setMuted] = useState(false);
  const [shareOpen, setShareOpen] = useState(false);
  const [windowSel, setWindowSel] = useState('0');
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const wrapRef = useRef<HTMLDivElement>(null);
  const busy = useRef(false);
  const lastSeq = useRef(0);
  const inputId = useRef<string | null>(null);
  /** 正在等待 peer_input_start 的返回：期间 Pointer Lock 已可能成功，不能误判为“已控制”。 */
  const startingControl = useRef(false);
  /** 操作启动代数：stop/屏幕断开后，迟到的 peer_input_start 结果必须丢弃。 */
  const controlSeq = useRef(0);
  /** 已真正建立操作会话（Pointer Lock 的异步成功事件不能只看 React 闭包状态）。 */
  const controlActive = useRef(false);
  const clipRef = useRef<string | null>(null);
  const audioIdRef = useRef<string | null>(null);
  const audioStopping = useRef(false);
  const startingAudio = useRef(false);
  const playerRef = useRef<AudioPlayer | null>(null);
  const cursorLayerRef = useRef<HTMLCanvasElement>(null);
  const shapeCanvasRef = useRef<HTMLCanvasElement | null>(null);
  const shapeKeyRef = useRef('');
  const cursorRef = useRef<{ x: number; y: number }>({ x: 0.5, y: 0.5 });
  const decoderRef = useRef<VideoDecoder | null>(null);
  const decoderPending = useRef<Promise<VideoDecoder | null> | null>(null);
  const decoderGen = useRef(0);
  const decoderSize = useRef('');
  const decoderCodec = useRef('');
  const decoderHd = useRef(false);
  const lastTs = useRef(0);
  const h264Broken = useRef(false);
  const decoderErrorShown = useRef(false);
  const prevCodec = useRef('');
  const [noFrame, setNoFrame] = useState(false);

  // 质量档位按设备记忆：进入会话时恢复并同步给服务端
  useEffect(() => {
    if (!s?.hostId) return;
    try {
      const raw = Number(localStorage.getItem(qualityKeyOf(s.hostId)));
      if (raw === 45 || raw === 65 || raw === 80) {
        setQuality(raw);
        if (sessionId && raw !== 65) {
          void api.peerScreenCommand(sessionId, { cmd: 'quality', q: raw }).catch(() => { /* ignore */ });
        }
      } else {
        setQuality(65);
      }
    } catch { /* ignore */ }
  }, [s?.hostId, sessionId]);

  // 服务端回报当前采集显示器：重连 / 远端切换后让选择器与实际源一致
  //（info 事件必须在这里被读取，不能只写进总线当死状态）。
  useEffect(() => {
    const idx = st?.info?.monitor;
    if (idx == null) return;
    setMonitor((m) => (m === idx ? m : idx));
  }, [st?.info?.monitor]);

  const closeDecoder = useCallback(() => {
    // 使在途的 ensureDecoder 作废：它 await capability 查询后不能复活一个
    // 已经关闭/切换会话的解码器。
    decoderGen.current += 1;
    decoderPending.current = null;
    try {
      decoderRef.current?.close();
    } catch { /* 已关闭 */ }
    decoderRef.current = null;
    decoderSize.current = '';
    decoderCodec.current = '';
    lastTs.current = 0;
  }, []);

  /**
   * 惰性创建 H.264 解码器。失败时永久回退 JPEG（避免反复重试拖慢画面）。
   * `codec` 来自关键帧 SPS（真实 profile/level），`hd` 决定是否声明 BT.709。
   * 输出直接画到 canvas；VideoFrame 必须 close，否则内存会持续增长。
   */
  const ensureDecoder = useCallback(async (codec?: string, hd?: boolean) => {
    if (h264Broken.current) return null;
    if (decoderRef.current) return decoderRef.current;
    if (typeof VideoDecoder === 'undefined') {
      h264Broken.current = true;
      return null;
    }
    // 单飞：isConfigSupported 是异步的，首帧期间第二个 tick 不能再造一个解码器。
    if (decoderPending.current) return decoderPending.current;
    const gen = decoderGen.current;
    const pending = (async () => {
      const cfg: VideoDecoderConfig = {
        ...DECODER_CONFIG,
        codec: codec || DECODER_CONFIG.codec,
        colorSpace: hd
          ? { primaries: 'bt709', transfer: 'bt709', matrix: 'bt709', fullRange: false }
          : { primaries: 'smpte170m', transfer: 'smpte170m', matrix: 'smpte170m', fullRange: false },
      };
      const support = await VideoDecoder.isConfigSupported(cfg).catch(() => null);
      if (gen !== decoderGen.current || h264Broken.current) return null;
      if (!support?.supported) {
        h264Broken.current = true;
        return null;
      }
      const dec = new VideoDecoder({
        output: (frame) => {
          const c = canvasRef.current;
          if (c) {
            if (c.width !== frame.displayWidth) c.width = frame.displayWidth;
            if (c.height !== frame.displayHeight) c.height = frame.displayHeight;
            c.getContext('2d')?.drawImage(frame, 0, 0);
          }
          frame.close();
        },
        error: (e) => {
          h264Broken.current = true;
          closeDecoder();
          if (!decoderErrorShown.current) {
            decoderErrorShown.current = true;
            showToast(T('err.peerScreenFail', { 0: String(e?.message ?? e) }));
          }
          if (sessionId) {
            void api.peerScreenCommand(sessionId, { cmd: 'codec', codec: 'jpeg' }).catch(() => { /* ignore */ });
          }
        },
      });
      dec.configure(cfg);
      if (gen !== decoderGen.current) {
        try { dec.close(); } catch { /* ignore */ }
        return null;
      }
      decoderRef.current = dec;
      decoderCodec.current = cfg.codec;
      decoderHd.current = !!hd;
      return dec;
    })();
    decoderPending.current = pending;
    try {
      return await pending;
    } finally {
      if (decoderPending.current === pending) decoderPending.current = null;
    }
  }, [sessionId, closeDecoder, showToast]);

  // 协商 H.264：WebCodecs 支持任一常见配置才请求，否则服务端保持 JPEG
  useEffect(() => {
    if (!sessionId) return;
    let cancelled = false;
    void (async () => {
      if (typeof VideoDecoder === 'undefined') return;
      let ok = false;
      for (const codec of DECODER_PROBES) {
        const r = await VideoDecoder.isConfigSupported({ ...DECODER_CONFIG, codec })
          .then((x) => !!x.supported)
          .catch(() => false);
        if (r) { ok = true; break; }
      }
      if (!cancelled && ok) {
        void api.peerScreenCommand(sessionId, { cmd: 'codec', codec: 'h264' }).catch(() => { /* ignore */ });
      }
    })();
    return () => { cancelled = true; };
  }, [sessionId]);

  // 屏幕通道 RTT：每 2s ping，回包由 screenBus 计算；再把量测值回报给服务端做自适应
  useEffect(() => {
    if (!sessionId) return;
    const timer = setInterval(() => {
      void api.peerScreenCommand(sessionId, { cmd: 'ping', t: Date.now() }).catch(() => { /* ignore */ });
    }, 2000);
    return () => clearInterval(timer);
  }, [sessionId]);
  useEffect(() => {
    if (!sessionId || st?.rttMs == null) return;
    void api.peerScreenCommand(sessionId, { cmd: 'stats', rttMs: st.rttMs }).catch(() => { /* ignore */ });
  }, [sessionId, st?.rttMs]);

  // 服务端回退 JPEG / 切换分辨率：关闭旧解码器（新码流会重新建）
  useEffect(() => {
    const c = st?.stream?.codec;
    if (!c) return;
    if (c === 'jpeg') {
      closeDecoder();
      if (prevCodec.current === 'h264' && st?.stream?.reason) {
        showToast(T('err.peerScreenFail', { 0: st.stream.reason }));
      }
    }
    prevCodec.current = c;
  }, [st?.stream?.codec, st?.stream?.reason, closeDecoder, showToast]);

  useEffect(() => {
    if (!sessionId) return;
    return subscribeScreen((id) => {
      if (id === sessionId) {
        setTick((t) => t + 1);
        if ((screenState(sessionId)?.frames ?? 0) > 0) setNoFrame(false);
      }
    });
  }, [sessionId]);

  // 首帧看门狗：通道 Success 后前端即标记 connected；编码器/采集卡住时画面会
  // 一直停在“等待画面”。10s 没帧就提示并自动回退 JPEG（排除 H.264 解码问题）。
  useEffect(() => {
    if (!sessionId) return;
    setNoFrame(false);
    const timer = setTimeout(() => {
      if ((screenState(sessionId)?.frames ?? 0) > 0) return;
      setNoFrame(true);
      void api.peerScreenCommand(sessionId, { cmd: 'codec', codec: 'jpeg' }).catch(() => { /* ignore */ });
    }, 10000);
    return () => clearTimeout(timer);
  }, [sessionId]);

  // 绘制：JPEG 解码成位图；H.264 送 VideoDecoder（输出回调里绘制）
  useEffect(() => {
    if (!sessionId) return;
    const state = screenState(sessionId);
    if (!state?.frame) return;
    // H.264 是参考链编码：按 screenBus 的队列逐帧送入，不能只处理最新帧。
    if (state.frame.codec === 'h264') {
      const batch = takeH264Frames(sessionId);
      if (!batch.length) return;
      void (async () => {
        for (const frame of batch) {
          if (frame.seq === lastSeq.current) continue;
          lastSeq.current = frame.seq;
          let bin: Uint8Array;
          try {
            bin = Uint8Array.from(atob(frame.data), (c) => c.charCodeAt(0));
          } catch {
            continue;
          }
          const sizeKey = `${frame.width}x${frame.height}`;
          const hd = frame.hd ?? frame.height >= 720;
          // 关键帧里的 SPS 决定真实 profile/level（分辨率切换后会变）
          const parsed = frame.key ? codecStringFromKeyframe(bin) : null;
          const wantCodec = parsed ?? decoderCodec.current;
          if (
            decoderRef.current &&
            (decoderSize.current !== sizeKey ||
              decoderHd.current !== hd ||
              (parsed !== null && decoderCodec.current !== '' && parsed !== decoderCodec.current))
          ) {
            if (!frame.key) continue; // 等新配置的关键帧
            closeDecoder();
          }
          const dec = await ensureDecoder(wantCodec || undefined, hd);
          if (!dec || dec.state === 'closed') continue;
          if (!frame.key && dec.state !== 'configured') continue;
          let ts = frame.tsUs || 0;
          if (ts <= lastTs.current) ts = lastTs.current + 1000;
          lastTs.current = ts;
          decoderSize.current = sizeKey;
          try {
            dec.decode(new EncodedVideoChunk({
              type: frame.key ? 'key' : 'delta',
              timestamp: ts,
              data: bin,
            }));
          } catch {
            /* 解码器忙/已关闭：丢弃这一帧，等下一次关键帧 */
          }
        }
      })();
      return;
    }
    const frame = state.frame;
    if (frame.seq === lastSeq.current) return;
    if (busy.current) return;
    lastSeq.current = frame.seq;
    busy.current = true;
    void (async () => {
      try {
        const bin = Uint8Array.from(atob(frame.data), (c) => c.charCodeAt(0));
        const bmp = await createImageBitmap(new Blob([bin], { type: 'image/jpeg' }));
        try {
          const c = canvasRef.current;
          if (c) {
            if (c.width !== frame.width) c.width = frame.width;
            if (c.height !== frame.height) c.height = frame.height;
            c.getContext('2d')?.drawImage(bmp, 0, 0);
          }
        } finally {
          bmp.close();
        }
      } catch {
        /* 坏帧直接丢弃，等下一帧 */
      }
      busy.current = false;
      const now = screenState(sessionId);
      if (now?.frame && now.frame.seq !== lastSeq.current) setTick((t) => t + 1);
    })();
  }, [tick, sessionId, ensureDecoder, closeDecoder]);

  // 远端光标形状：叠加层独立于视频帧，避免每帧重绘
  useEffect(() => {
    const layer = cursorLayerRef.current;
    const main = canvasRef.current;
    if (!layer || !main) return;
    if (layer.width !== main.width) layer.width = main.width;
    if (layer.height !== main.height) layer.height = main.height;
    const ctx = layer.getContext('2d');
    if (!ctx) return;
    ctx.clearRect(0, 0, layer.width, layer.height);
    const c = st?.cursor;
    if (!c?.visible || !c.shape || !c.shape.rgba) return;
    // 远端数据不可信：非法尺寸/长度会让 ImageData 同步抛错，而这是在
    // useEffect 里，没有 ErrorBoundary 时整棵 React 树会被卸载。
    const w = Number(c.shape.width);
    const h = Number(c.shape.height);
    if (!Number.isInteger(w) || !Number.isInteger(h) || w <= 0 || h <= 0 || w > 512 || h > 512) {
      return;
    }
    const key = `${w}x${h}:${c.shape.rgba}`;
    if (shapeKeyRef.current !== key) {
      try {
        const bin = Uint8Array.from(atob(c.shape.rgba), (ch) => ch.charCodeAt(0));
        if (bin.length !== w * h * 4) return;
        const off = shapeCanvasRef.current ?? document.createElement('canvas');
        off.width = w;
        off.height = h;
        const octx = off.getContext('2d');
        if (!octx) return;
        octx.putImageData(new ImageData(new Uint8ClampedArray(bin), w, h), 0, 0);
        shapeCanvasRef.current = off;
        shapeKeyRef.current = key;
      } catch {
        return;
      }
    }
    const off = shapeCanvasRef.current;
    if (!off) return;
    const scale = c.srcWidth > 0 ? layer.width / c.srcWidth : 1;
    const x = c.nx * layer.width - c.shape.hotspotX * scale;
    const y = c.ny * layer.height - c.shape.hotspotY * scale;
    ctx.imageSmoothingEnabled = true;
    ctx.drawImage(off, x, y, w * scale, h * scale);
  }, [st?.cursor, tick]);

  // 切回桌面时重置窗口下拉
  useEffect(() => {
    if (st?.source?.source === 'desktop') setWindowSel('0');
  }, [st?.source?.source]);

  // Pointer Lock 状态跟随浏览器；控制尚未真正建立时误锁立即释放，
  // 否则 peer_input_start 失败后本机鼠标会被锁在画布上。
  useEffect(() => {
    const onChange = () => {
      const isLocked = document.pointerLockElement === canvasRef.current;
      setLocked(isLocked);
      // 点击「开始操作」后 lock 成功可能与 IPC 返回/屏幕断开交错；
      // 用 ref 判断是否仍有有效会话，避免 stop 后迟到的 lock 把鼠标锁住。
      if (isLocked && !controlActive.current && !startingControl.current) {
        void document.exitPointerLock();
      }
    };
    document.addEventListener('pointerlockchange', onChange);
    return () => document.removeEventListener('pointerlockchange', onChange);
  }, []);

  const releasePointerLock = useCallback(() => {
    // 组件卸载时 canvasRef 可能已为 null；按元素类型兜底释放 Pointer Lock，
    // 否则断开后本机鼠标仍会被锁在旧画布上（表现为移动卡顿/光标消失）。
    const lockedEl = document.pointerLockElement as HTMLElement | null;
    if (lockedEl && (lockedEl === canvasRef.current || lockedEl.classList.contains('rv-canvas'))) {
      void document.exitPointerLock();
    }
  }, []);

  const stopControl = useCallback(() => {
    controlSeq.current += 1;
    controlActive.current = false;
    startingControl.current = false;
    if (inputId.current) {
      void api.peerInputStop(inputId.current).catch(() => { /* ignore */ });
      inputId.current = null;
    }
    setControlling(false);
    setLocked(false);
    setCursor(null);
    releasePointerLock();
  }, [releasePointerLock]);

  // 失去窗口焦点时无法保证收到 keyup/mouseup；关闭输入通道让服务端
  // HeldInput::Drop 补发释放，避免远端一直按住 Ctrl/鼠标左键。
  useEffect(() => {
    const onBlur = () => {
      if (controlActive.current) stopControl();
    };
    const onVisibility = () => {
      if (document.visibilityState === 'hidden' && controlActive.current) stopControl();
    };
    window.addEventListener('blur', onBlur);
    document.addEventListener('visibilitychange', onVisibility);
    return () => {
      window.removeEventListener('blur', onBlur);
      document.removeEventListener('visibilitychange', onVisibility);
    };
  }, [stopControl]);

  useEffect(() => {
    if (controlling) startingControl.current = false;
  }, [controlling]);

  // 屏幕断开/报错时释放硬件解码器，避免标签还开着但解码器/GPU 会话残留。
  useEffect(() => {
    if (s?.status !== 'connected' || st?.error) closeDecoder();
  }, [s?.status, st?.error, closeDecoder]);

  // 屏幕断开/报错/授权撤销/输入通道单独结束时，必须立刻退出操作模式：
  // 否则 window 键鼠监听和 Pointer Lock 会继续拦截本机输入，表现为键鼠卡顿。
  useEffect(() => {
    // startingControl 也要覆盖：peer_input_start 尚未返回时屏幕断开，
    // 迟到的成功结果必须作废，Pointer Lock 也必须释放。
    if (!controlling && !startingControl.current) return;
    // inputId 防止“停止后立刻重开”时旧通道的 ended 事件误停新通道。
    const currentInputEnded =
      controlling &&
      inputEndedId !== undefined &&
      (inputEndedId === '' || inputEndedId === inputId.current);
    if (
      currentInputEnded ||
      s?.status !== 'connected' ||
      st?.error ||
      st?.auth?.allowInput === false
    ) {
      stopControl();
    }
  }, [controlling, inputEndedId, s?.status, st?.error, st?.auth?.allowInput, stopControl]);

  const stopClipboard = useCallback(() => {
    if (clipRef.current) {
      void api.peerClipboardStop(clipRef.current).catch(() => { /* ignore */ });
      clipRef.current = null;
    }
    setClipId(null);
  }, []);

  // 对端断开 / 授权撤销会结束剪贴板通道：不主动收起工具栏状态的话，
  // 按钮会一直显示「已开启」，后续拉取/推送要等 5s 超时才报错。
  useEffect(() => {
    if (!sessionId || clipEndedId === undefined) return;
    if (clipId && clipId === clipEndedId) {
      clipRef.current = null;
      setClipId(null);
      showToast(T('rd.clipEnded'));
    }
    clearClipboardEnded(sessionId);
  }, [sessionId, clipEndedId, clipId, clearClipboardEnded, showToast]);

  const stopAudio = useCallback(() => {    audioStopping.current = true;
    const id = audioIdRef.current;
    audioIdRef.current = null;
    if (id) void api.peerAudioStop(id).catch(() => { /* ignore */ });
    playerRef.current?.stop();
    playerRef.current = null;
    setAudioOn(false);
    setMicOn(false);
    setAudioStats((s) => ({ ...s, decoded: 0, peak: 0 }));
    window.setTimeout(() => { audioStopping.current = false; }, 800);
  }, []);

  // 屏幕链路终止时一并关闭附加通道，避免标签还在但音频/剪贴板连接残留。
  useEffect(() => {
    if (s?.status !== 'connected' || st?.error) {
      stopClipboard();
      stopAudio();
    }
  }, [s?.status, st?.error, stopClipboard, stopAudio]);

  const startAudio = useCallback(async () => {
    if (!sessionId || startingAudio.current) return;
    if (typeof AudioDecoder === 'undefined') {
      showToast(T('rd.audioUnsupported'));
      return;
    }
    startingAudio.current = true;
    let player: AudioPlayer | null = null;
    try {
      player = new AudioPlayer(
        (s) => setAudioStats(s),
        (detail) => {
          showToast(T('err.peerAudioFail', { 0: detail }));
          stopAudio();
        },
      );
      const ok = await player.start();
      if (!ok) {
        player.stop();
        player = null;
        showToast(T('rd.audioUnsupported'));
        return;
      }
      player.setVolume(volume);
      player.setMuted(muted);
      playerRef.current = player;
      const ch = api.audioFrameChannel((f) => {
        if (f.kind === 'data') {
          player?.push(f.data, f.seq);
        } else if (!audioStopping.current) {
          showToast(T('err.peerAudioFail', { 0: f.detail }));
          stopAudio();
        }
      });
      const r = await api.peerAudioStart(sessionId, st?.auth?.token ?? null, ch);
      audioIdRef.current = r.id;
      setAudioOn(true);
      setAudioStats({ decoded: 0, peak: 0, rate: r.rate, channels: r.channels });
      showToast(T('rd.audioOn'));
    } catch (e) {
      player?.stop();
      if (playerRef.current === player) playerRef.current = null;
      showToast(T('err.peerAudioFail', {
        0: String((e as { detail?: string })?.detail ?? (e as Error)?.message ?? T('err.unknown')),
      }));
    } finally {
      startingAudio.current = false;
    }
  }, [sessionId, st?.auth?.token, volume, muted, showToast, stopAudio]);

  /** 会话内开关麦克风混音：服务端懒加载默认录音设备，关闭即释放。 */
  const toggleMic = useCallback(async () => {
    const id = audioIdRef.current;
    if (!id) return;
    const next = !micOn;
    try {
      const ok = await api.peerAudioSetMic(id, next);
      if (!ok) {
        showToast(T('rd.micUnavailable'));
        return;
      }
      setMicOn(next);
      showToast(next ? T('rd.micOn') : T('rd.micOff'));
    } catch {
      showToast(T('rd.micUnavailable'));
    }
  }, [micOn, showToast]);

  useEffect(() => {
    // 切换 screen 标签会复用同一个 RemoteView 实例：上一台设备的失败标记、
    // 显示器索引不能带到下一台设备。
    h264Broken.current = false;
    decoderErrorShown.current = false;
    prevCodec.current = '';
    setMonitor(0);
    stopControl();
    stopClipboard();
    stopAudio();
    closeDecoder();
    return () => {
      stopControl();
      stopClipboard();
      stopAudio();
      closeDecoder();
    };
  }, [sessionId, stopControl, stopClipboard, stopAudio, closeDecoder]);

  const sendInput = useCallback((ev: Record<string, unknown>) => {
    if (inputId.current) void api.peerInputSend(inputId.current, ev).catch(() => { /* ignore */ });
  }, []);

  // 键盘：控制模式下捕获（Esc 退出；工具栏控件保持可用）
  useEffect(() => {
    if (!controlling || !sessionId) return;
    const onKey = (e: KeyboardEvent, down: boolean) => {
      const target = e.target as HTMLElement | null;
      if (target instanceof HTMLElement && target.closest('select,button,input')) return;
      if (e.key === 'Escape' && down) {
        e.preventDefault();
        stopControl();
        return;
      }
      const vk = vkOf(e.code);
      if (vk != null) {
        e.preventDefault();
        sendInput({ type: 'key', vk, down });
        return;
      }
      if (down && e.key.length === 1 && !e.ctrlKey && !e.altKey && !e.metaKey) {
        e.preventDefault();
        sendInput({ type: 'text', s: e.key });
      }
    };
    const kd = (e: KeyboardEvent) => onKey(e, true);
    const ku = (e: KeyboardEvent) => onKey(e, false);
    window.addEventListener('keydown', kd);
    window.addEventListener('keyup', ku);
    return () => {
      window.removeEventListener('keydown', kd);
      window.removeEventListener('keyup', ku);
    };
  }, [controlling, sessionId, sendInput, stopControl]);

  // 旧版对端不支持「查看并操作」：提示一次（已按仅查看打开）
  useEffect(() => {
    const a = st?.auth;
    if (!a || inputUnsupportedWarned.current) return;
    if (a.inputRequested && a.inputSupported === false) {
      inputUnsupportedWarned.current = true;
      showToast(T('rd.inputUnsupported'));
    }
  }, [st?.auth, showToast]);

  // 会话中申请操作权限：成功（allowInput 变 true）或 35s 后复位
  useEffect(() => {
    if (st?.auth?.allowInput && requestingInput) setRequestingInput(false);
  }, [st?.auth?.allowInput, requestingInput]);
  useEffect(() => {
    if (!requestingInput) return;
    const timer = setTimeout(() => setRequestingInput(false), 35000);
    return () => clearTimeout(timer);
  }, [requestingInput]);

  if (!sessionId) return null;
  const error = st?.error;
  const allowInput = st?.auth?.allowInput ?? false;
  const allowClipboard = st?.auth?.clipboard ?? false;
  const codecNow = st?.stream?.codec ?? st?.frame?.codec ?? 'jpeg';
  const captureStateNow = st?.captureState?.state ?? 'live';
  const captureWarn = captureWarnText(captureStateNow);

  const send = (cmd: Record<string, unknown>) => {
    void api.peerScreenCommand(sessionId, cmd).catch(() => {
      showToast(T('err.peerScreenFail', { 0: T('rd.disconnected') }));
    });
  };

  const requestInput = () => {
    setRequestingInput(true);
    void api.peerScreenCommand(sessionId, { cmd: 'requestInput' }).catch(() => {
      setRequestingInput(false);
      showToast(T('err.peerScreenFail', { 0: T('rd.disconnected') }));
    });
  };

  const startControl = async (seq: number) => {
    try {
      const id = await api.peerInputStart(sessionId, st?.auth?.token ?? null);
      // 启动期间屏幕断开/用户取消：丢弃迟到的结果并关闭刚建立的后端通道。
      if (seq !== controlSeq.current) {
        void api.peerInputStop(id).catch(() => { /* ignore */ });
        return;
      }
      // 通道可能在 IPC 返回前就已断开（ended 事件带同一个 inputId），不能清掉后假装在控制。
      if (sessionId && useAppStore.getState().inputEnded[sessionId] === id) {
        startingControl.current = false;
        releasePointerLock();
        void api.peerInputStop(id).catch(() => { /* ignore */ });
        return;
      }
      inputId.current = id;
      controlActive.current = true;
      if (sessionId) clearInputEnded(sessionId);
      setControlling(true);
      cursorRef.current = { x: 0.5, y: 0.5 };
      setCursor({ x: 0.5, y: 0.5 });
      canvasRef.current?.focus();
    } catch (e) {
      if (seq !== controlSeq.current) return;
      startingControl.current = false;
      // 工具栏在用户手势里已请求 Pointer Lock；输入授权失败时必须释放。
      releasePointerLock();
      showToast(T('err.peerInputFail', {
        0: T('err.unknown') + ' — ' + String((e as { detail?: string })?.detail ?? (e as Error)?.message ?? ''),
      }));
    }
  };

  const clipError = (e: unknown) =>
    T('err.peerClipboardFail', {
      0: String((e as { detail?: string })?.detail ?? (e as Error)?.message ?? T('err.unknown')),
    });

  /** 剪贴板负载是否有可写内容（空字符串按空处理，避免用空文本覆盖本机剪贴板）。 */
  const hasClipPayload = (c: api.ClipboardFormats | null | undefined): boolean =>
    !!(c && (c.text || c.html || c.rtf || c.png || c.dib));

  const startClipboard = async () => {
    setClipBusy(true);
    try {
      const r = await api.peerClipboardStart(sessionId, st?.auth?.token ?? null);
      clipRef.current = r.id;
      setClipId(r.id);
      // 开启即同步一次远端 → 本机（用户显式点击，语义明确）。
      // 富格式优先；旧后端没有 local_set_rich 时回退旧命令写文本。
      const content: api.ClipboardFormats = { ...(r.formats ?? {}) };
      if (r.text && !content.text) content.text = r.text;
      if (hasClipPayload(content)) {
        const ok = await api.clipboardLocalSetRich(content).catch(() => false);
        if (!ok && r.text) await api.clipboardLocalSet(r.text).catch(() => false);
      }
      showToast(T('rd.clipOn'));
    } catch (e) {
      showToast(clipError(e));
    } finally {
      setClipBusy(false);
    }
  };

  const pullClipboard = async () => {
    const id = clipRef.current;
    if (!id) return;
    setClipBusy(true);
    try {
      // 富格式优先；命令不可用（旧后端）时回退纯文本 + 旧命令。
      const rich = await api.peerClipboardGetRich(id).catch(() => null);
      if (rich) {
        if (!hasClipPayload(rich)) {
          showToast(T('rd.clipEmpty'));
          return;
        }
        const ok = await api.clipboardLocalSetRich(rich).catch(() => false);
        if (!ok) {
          // 富格式整体写入失败时，至少把文本写进去（与旧行为一致）。
          if (!rich.text) throw new Error(T('err.peerClipboardWrite'));
          await api.clipboardLocalSet(rich.text);
        }
        showToast(T('rd.clipPulled'));
        return;
      }
      const text = await api.peerClipboardGet(id);
      await api.clipboardLocalSet(text);
      showToast(T('rd.clipPulled'));
    } catch (e) {
      showToast(clipError(e));
    } finally {
      setClipBusy(false);
    }
  };

  const pushClipboard = async () => {
    const id = clipRef.current;
    if (!id) return;
    setClipBusy(true);
    try {
      // 读本机富格式发送；旧后端没有 local_get_rich 时回退旧命令读文本。
      const rich = await api.clipboardLocalGetRich().catch(() => null);
      if (rich && hasClipPayload(rich)) {
        const ok = await api.peerClipboardSetRich(id, rich).catch(() => false);
        if (!ok && rich.text) {
          // 旧被控端 / 富格式发送失败：落回纯文本旧命令。
          await api.peerClipboardSet(id, rich.text);
        } else if (!ok) {
          throw new Error(T('err.peerClipboardWrite'));
        }
      } else {
        const text = await api.clipboardLocalGet();
        await api.peerClipboardSet(id, text);
      }
      showToast(T('rd.clipPushed'));
    } catch (e) {
      showToast(clipError(e));
    } finally {
      setClipBusy(false);
    }
  };

  const onMove = (e: React.MouseEvent<HTMLCanvasElement>) => {
    if (!controlling) return;
    const c = canvasRef.current;
    if (!c) return;
    const r = c.getBoundingClientRect();
    if (r.width <= 0 || r.height <= 0) return;
    let nx: number;
    let ny: number;
    if (locked) {
      // Pointer Lock：用相对位移累加（本地cursorRef），不会被工具栏/边缘夹住
      nx = clamp01(cursorRef.current.x + e.movementX / r.width);
      ny = clamp01(cursorRef.current.y + e.movementY / r.height);
    } else {
      nx = (e.clientX - r.left) / r.width;
      ny = (e.clientY - r.top) / r.height;
      if (nx < 0 || nx > 1 || ny < 0 || ny > 1) return;
    }
    cursorRef.current = { x: nx, y: ny };
    setCursor({ x: nx, y: ny });
    sendInput({ type: 'move', x: nx, y: ny, monitor });
  };

  const buttonName = (b: number) => (b === 2 ? 'right' : b === 1 ? 'middle' : 'left');

  return (
    <div
      className="remoteview"
      data-screen-session={sessionId}
      data-screen-frames={st?.frames ?? 0}
      data-screen-size={st?.frame ? `${st.frame.width}x${st.frame.height}` : ''}
      data-screen-locked={locked ? '1' : '0'}
      data-clipboard-id={clipId ?? ''}
      data-screen-codec={codecNow}
      data-screen-fps={st?.stream?.fps ?? 0}
      data-screen-bitrate={st?.stream?.bitrate ?? 0}
      data-screen-stream-width={st?.stream?.width ?? 0}
      data-screen-stream-height={st?.stream?.height ?? 0}
      data-audio-state={audioOn ? 'on' : 'off'}
      data-audio-mic={micOn ? '1' : '0'}
      data-audio-decoded={audioStats.decoded}
      data-audio-peak={audioStats.peak.toFixed(3)}
      data-audio-rate={audioStats.rate}
      data-audio-muted={muted ? '1' : '0'}
      data-screen-source={st?.source?.source ?? 'desktop'}
      data-screen-cursor={st?.cursor?.visible ? '1' : '0'}
      data-screen-cursor-shape={st?.cursor?.shape ? '1' : '0'}
    >
      <div className="rv-toolbar">
        <span className="rv-title">
          <Icon n="monitor" s={14} />
          {s?.title ?? T('rd.title')}
        </span>
        <label className="rv-ctl">
          {T('rd.quality')}
          <select
            data-screen-quality
            value={quality}
            onChange={(e) => {
              const q = Number(e.target.value);
              setQuality(q);
              if (s?.hostId) {
                try { localStorage.setItem(qualityKeyOf(s.hostId), String(q)); } catch { /* ignore */ }
              }
              send({ cmd: 'quality', q });
            }}
          >
            {QUALITIES.map((q) => (
              <option key={q} value={q}>{T(qualityKey(q))}</option>
            ))}
          </select>
        </label>
        <label className="rv-ctl">
          {T('rd.monitor')}
          <select
            data-screen-monitor
            value={monitor}
            onChange={(e) => {
              const idx = Number(e.target.value);
              setMonitor(idx);
              send({ cmd: 'monitor', index: idx });
            }}
          >
            {(st?.monitors ?? []).map((m) => (
              <option key={m.index} value={m.index}>
                {m.name} · {m.width}×{m.height}
              </option>
            ))}
          </select>
        </label>
        <button
          className="tbtn"
          data-screen-zoom
          onClick={() => setZoom((z) => (z === 'fit' ? 'original' : 'fit'))}
        >
          {zoom === 'fit' ? T('rd.zoomFit') : T('rd.zoomOriginal')}
        </button>
        <button
          className="tbtn"
          data-screen-fullscreen
          onClick={() => {
            const el = wrapRef.current;
            if (!el) return;
            if (document.fullscreenElement) void document.exitFullscreen();
            else void el.requestFullscreen?.().catch(() => { /* 平台不支持时忽略 */ });
          }}
        >
          {T('rd.fullscreen')}
        </button>
        <button
          className={'tbtn' + (shareOpen ? ' on' : '')}
          data-screen-window-toggle
          onClick={() => {
            setShareOpen((v) => !v);
            if (!shareOpen) send({ cmd: 'windows' });
          }}
        >
          {T('rd.window')}
        </button>
        {shareOpen && (
          <label className="rv-ctl">
            {T('rd.windowSource')}
            <select
              data-screen-window
              value={windowSel}
              onChange={(e) => {
                const v = e.target.value;
                setWindowSel(v);
                if (v === '0') {
                  send({ cmd: 'desktop' });
                } else {
                  const item = (st?.windows ?? []).find((w) => String(w.hwnd) === v);
                  send({ cmd: 'window', hwnd: Number(v), title: item?.title ?? '' });
                }
              }}
            >
              <option value="0">{T('rd.windowDesktop')}</option>
              {(st?.windows ?? []).map((w) => (
                <option key={w.hwnd} value={String(w.hwnd)}>
                  {w.title.length > 48 ? `${w.title.slice(0, 48)}…` : w.title}
                </option>
              ))}
            </select>
          </label>
        )}
        {st?.source?.source === 'window' && (
          <span className="rv-source-hint" data-screen-window-hint>
            {T('rd.windowHint')}
          </span>
        )}
        <span className="rv-stats mono" data-screen-stats>
          {st?.frame ? `${st.frame.width}×${st.frame.height} · ${Math.round(st.frame.bytes / 1024)}KB` : T('rd.waiting')}
          {st?.stream ? ` · ${st.stream.codec.toUpperCase()} ${st.stream.fps}fps` : ''}
          {st?.stream && st.stream.bitrate > 0 ? ` · ${Math.round(st.stream.bitrate / 1000)}kbps` : ''}
          {' · '}{st?.frames ?? 0}
          {st?.rttMs != null ? ` · RTT ${st.rttMs}ms` : ''}
          {controlling && rtt != null ? ` · ${T('rd.inputRtt')} ${rtt}ms` : ''}
        </span>
        {codecNow === 'h264' && (
          <button className="tbtn" data-screen-key onClick={() => send({ cmd: 'keyframe' })}>
            {T('rd.resync')}
          </button>
        )}
        {allowClipboard && (
          <button
            className={'tbtn' + (clipId ? ' on' : '')}
            data-screen-clipboard
            disabled={clipBusy}
            onClick={() => (clipId ? stopClipboard() : void startClipboard())}
          >
            {clipId ? T('rd.clipOff') : T('rd.clipboard')}
          </button>
        )}
        {clipId && (
          <>
            <button className="tbtn" data-clip-pull disabled={clipBusy} onClick={() => void pullClipboard()}>
              {T('rd.clipPull')}
            </button>
            <button className="tbtn" data-clip-push disabled={clipBusy} onClick={() => void pushClipboard()}>
              {T('rd.clipPush')}
            </button>
          </>
        )}
        {st?.auth?.audio && (
          <button
            className={'tbtn' + (audioOn ? ' on' : '')}
            data-screen-audio
            onClick={() => (audioOn ? stopAudio() : void startAudio())}
          >
            {audioOn ? T('rd.audioOff') : T('rd.audio')}
          </button>
        )}
        {audioOn && (
          <>
            <label className="rv-ctl">
              {T('rd.volume')}
              <input
                className="rv-vol"
                data-audio-volume
                type="range"
                min={0}
                max={1}
                step={0.05}
                value={volume}
                onChange={(e) => {
                  const v = Number(e.target.value);
                  setVolume(v);
                  playerRef.current?.setVolume(v);
                }}
              />
            </label>
            <button
              className="tbtn"
              data-audio-mute
              onClick={() => {
                const m = !muted;
                setMuted(m);
                playerRef.current?.setMuted(m);
              }}
            >
              {muted ? T('rd.unmute') : T('rd.mute')}
            </button>
            <button
              className={'tbtn' + (micOn ? ' on' : '')}
              data-audio-mic-toggle
              onClick={() => void toggleMic()}
            >
              {T('rd.mic')}
            </button>
          </>
        )}
        {!allowInput && st?.auth?.token && st.auth.inputSupported !== false && (
          <button
            className={'tbtn' + (requestingInput ? ' on' : '')}
            data-screen-request-input
            disabled={requestingInput}
            onClick={requestInput}
          >
            {requestingInput ? T('rd.requesting') : T('rd.requestControl')}
          </button>
        )}
        {allowInput && (
          <button
            className={'tbtn' + (controlling ? ' on' : '')}
            data-screen-control
            onClick={() => {
              if (controlling) { stopControl(); return; }
              if (startingControl.current) return;
              startingControl.current = true;
              const seq = ++controlSeq.current;
              void startControl(seq);
              // Pointer Lock 必须在用户手势内请求（异步结果不影响本次调用）
              canvasRef.current?.requestPointerLock?.();
            }}
          >
            {controlling ? T('rd.controlStop') : T('rd.controlStart')}
          </button>
        )}
        <button
          className="tbtn"
          data-screen-close
          onClick={() => {
            stopControl();
            dropScreen(sessionId);
            void closeSession(sessionId);
          }}
        >
          {T('rd.disconnect')}
        </button>
      </div>
      <div
        className={'rv-canvas-wrap' + (zoom === 'original' ? ' original' : '')}
        ref={wrapRef}
        data-screen-wrap
      >
        <div className={'rv-canvas-box' + (zoom === 'original' ? ' original' : '')}>
          <canvas
            ref={canvasRef}
            className={'rv-canvas' + (zoom === 'original' ? ' original' : '')}
            tabIndex={controlling ? 0 : -1}
            style={controlling ? { cursor: locked ? 'none' : 'crosshair' } : undefined}
            onMouseMove={onMove}
            onMouseDown={(e) => {
              if (!controlling) return;
              e.preventDefault();
              // Pointer Lock 是增强项，不是点击的前提：被策略拒绝/处于
              // Esc 冷却期时仍必须把按键发给远端。
              if (!locked) void canvasRef.current?.requestPointerLock?.();
              sendInput({ type: 'button', button: buttonName(e.button), down: true });
            }}
            onMouseUp={(e) => {
              if (!controlling) return;
              e.preventDefault();
              sendInput({ type: 'button', button: buttonName(e.button), down: false });
            }}
            onContextMenu={(e) => {
              if (controlling) e.preventDefault();
            }}
            onWheel={(e) => {
              if (!controlling) return;
              e.preventDefault();
              sendInput({ type: 'wheel', delta: e.deltaY < 0 ? 120 : -120 });
            }}
          />
          <canvas ref={cursorLayerRef} className="rv-cursor-layer" data-cursor-layer />
          {controlling && cursor && !st?.cursor?.visible && (
            <span
              className="rv-cursor"
              data-screen-cursor
              style={{ left: `${cursor.x * 100}%`, top: `${cursor.y * 100}%` }}
            />
          )}
        </div>
        {captureWarn && (
          <div className="rv-capture-warn" data-capture-state={captureStateNow}>
            {captureWarn}
          </div>
        )}
        {error && <div className="rv-error">{error}</div>}
        {!st?.frame && !error && <div className="rv-waiting">{T('rd.waitingHint')}</div>}
        {noFrame && !st?.frame && !error && (
          <div className="rv-noframe" data-screen-noframe>
            <div>{T('rd.noFrame', { 0: '10' })}</div>
            <button
              className="tbtn"
              data-screen-retry-jpeg
              onClick={() => {
                setNoFrame(false);
                void api.peerScreenCommand(sessionId, { cmd: 'codec', codec: 'jpeg' }).catch(() => { /* ignore */ });
                void api.peerScreenCommand(sessionId, { cmd: 'keyframe' }).catch(() => { /* ignore */ });
              }}
            >
              {T('rd.switchJpeg')}
            </button>
          </div>
        )}
        {controlling && !locked && <div className="rv-control-hint">{T('rd.lockHint')}</div>}
      </div>
    </div>
  );
}
