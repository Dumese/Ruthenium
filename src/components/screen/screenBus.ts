// 屏幕帧总线路：后端 ipc Channel 的帧只保留「最新一帧」，组件按需取用。
//
// 为什么不进 zustand：JPEG/H.264 帧（base64）体量大且高频，塞进全局 store 会触发整棵树
// 重渲染；这里只通知「这个 session 有新帧」，绘制端按 codec 自己解码并节流。
import type { CursorShapeInfo, MonitorInfo, ScreenFrame, WindowInfo } from '../../ipc/commands';

export interface AudioStats {
  decoded: number;
  peak: number;
  rate: number;
  channels: number;
}

export interface StreamInfo {
  codec: 'jpeg' | 'h264';
  fps: number;
  bitrate: number;
  width: number;
  height: number;
  reason?: string | null;
}

export interface ScreenFrameData {
  width: number;
  height: number;
  data: string;
  bytes: number;
  codec: 'jpeg' | 'h264';
  key: boolean;
  /** 源画面是否 HD（服务端协议 flags bit2） */
  hd?: boolean;
  tsUs: number;
  seq: number;
}

export interface ScreenState {
  frame: ScreenFrameData | null;
  /** H.264 是参考链编码，不能只保留最新一帧；队列按到达顺序交给解码器。 */
  h264Queue: ScreenFrameData[];
  monitors: MonitorInfo[];
  info: { monitor: number; width: number; height: number } | null;
  error: string | null;
  frames: number;
  bytes: number;
  monitor: number;
  /** 输入鉴权（服务端在流开始时下发；申请操作成功后也会再发一次） */
  auth: {
    token: string;
    allowInput: boolean;
    clipboard: boolean;
    audio: boolean;
    inputSupported: boolean;
    inputRequested: boolean;
  } | null;
  /** 服务端下发的编码/自适应状态 */
  stream: StreamInfo | null;
  /** 屏幕通道 RTT（查看端 ping/pong，毫秒） */
  rttMs: number | null;
  /** 远端光标（桌面采集） */
  cursor: {
    visible: boolean;
    nx: number;
    ny: number;
    srcWidth: number;
    srcHeight: number;
    shape: CursorShapeInfo | null;
  } | null;
  /** 可共享窗口列表 */
  windows: WindowInfo[];
  /** 当前采集源 */
  source: { source: 'desktop' | 'window'; title?: string | null; reason?: string | null };
}

const states = new Map<string, ScreenState>();
const listeners = new Set<(id: string) => void>();
let seq = 1;

function ensure(id: string): ScreenState {
  let s = states.get(id);
  if (!s) {
    s = {
      frame: null,
      h264Queue: [],
      monitors: [],
      info: null,
      error: null,
      frames: 0,
      bytes: 0,
      monitor: 0,
      auth: null,
      stream: null,
      rttMs: null,
      cursor: null,
      windows: [],
      source: { source: 'desktop' },
    };
    states.set(id, s);
  }
  return s;
}

export function screenState(id: string): ScreenState | undefined {
  return states.get(id);
}

export function subscribeScreen(fn: (id: string) => void): () => void {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

function notify(id: string) {
  for (const fn of listeners) fn(id);
}

export function pushScreen(f: ScreenFrame): void {
  const s = ensure(f.sessionId);
  switch (f.kind) {
    case 'frame': {
      const next: ScreenFrameData = {
        width: f.width,
        height: f.height,
        data: f.data,
        bytes: f.bytes,
        codec: f.codec,
        key: f.key,
        hd: f.hd,
        tsUs: f.tsUs,
        seq: seq++,
      };
      s.frame = next;
      if (next.codec === 'h264') {
        s.h264Queue.push(next);
        // 上限只是防内存失控；正常编码/传输不会接近。若溢出则丢弃最旧帧，
        // 解码器会因缺参考而自行等待下一个 IDR（服务端 2s GOP / 5s 补帧）。
        if (s.h264Queue.length > 240) s.h264Queue.splice(0, s.h264Queue.length - 240);
      } else {
        s.h264Queue.length = 0;
      }
      s.frames += 1;
      s.bytes += f.bytes;
      s.error = null;
      break;
    }
    case 'monitors':
      s.monitors = f.items;
      break;
    case 'info':
      s.info = { monitor: f.monitor, width: f.width, height: f.height };
      s.monitor = f.monitor;
      break;
    case 'error':
      s.error = f.detail;
      break;
    case 'auth':
      s.auth = {
        token: f.token,
        allowInput: f.allowInput,
        clipboard: f.clipboard,
        audio: f.audio,
        inputSupported: f.inputSupported ?? true,
        inputRequested: f.inputRequested ?? false,
      };
      break;
    case 'pong':
      s.rttMs = Math.max(0, Math.min(60_000, Date.now() - f.t));
      break;
    case 'cursor':
      s.cursor = {
        visible: f.visible,
        nx: f.nx,
        ny: f.ny,
        srcWidth: f.srcWidth,
        srcHeight: f.srcHeight,
        shape: f.shape ?? s.cursor?.shape ?? null,
      };
      break;
    case 'windows':
      s.windows = f.items;
      break;
    case 'source':
      s.source = { source: f.source, title: f.title, reason: f.reason };
      break;
    case 'stream':
      s.stream = {
        codec: f.codec,
        fps: f.fps,
        bitrate: f.bitrate,
        width: f.width,
        height: f.height,
        reason: f.reason ?? null,
      };
      break;
  }
  notify(f.sessionId);
}

/** 取走当前排队的 H.264 帧（按到达顺序）；JPEG 仍只看最新帧。 */
export function takeH264Frames(id: string): ScreenFrameData[] {
  const s = states.get(id);
  if (!s || s.h264Queue.length === 0) return [];
  return s.h264Queue.splice(0, s.h264Queue.length);
}

export function dropScreen(id: string): void {
  states.delete(id);
}
