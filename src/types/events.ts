// 事件名与 payload 类型
import type { Transfer, HostMetrics } from './models';

/** term://data：data 是终端字节的 base64（STANDARD alphabet），前端解码成 Uint8Array 再写 xterm。 */
export interface TermDataEvent { sessionId: string; data: string }
export interface TermExitEvent { sessionId: string; reason: string }
export interface ConnStateEvent { sessionId: string; state: 'connecting'|'connected'|'disconnected';
  latencyMs?: number;
  /** 断线原因：lost 才允许自动重连；closed_by_user 不重连 */
  reason?: 'lost'|'closed_by_user' }
export interface HostkeyChallengeEvent { sessionId: string; hostId: string; host: string; port: number; algo: string; fingerprint: string; changed: boolean }
export interface SftpProgressEvent { transferId: string; bytesDone: number; bytesTotal: number; state: Transfer['state'] }
export interface SftpChangedEvent { sessionId: string; side: 'local'|'remote'; path: string }
export interface TunnelStateEvent { ruleId: string; active: boolean; error?: string }
export type MetricsSampleEvent = HostMetrics;
export interface AppErrorEvent { code: string; detail: string; sessionId?: string }
/** 输入通道延迟（peer://input-stats）。 */
export interface InputStatsEvent { sessionId: string; rttMs: number; ended?: boolean; inputId?: string }
/** 查看端剪贴板通道结束（peer://clipboard）。 */
export interface ClipboardEndedEvent { sessionId: string; id: string }

export const EV = {
  termData: 'term://data',
  termExit: 'term://exit',
  connState: 'conn://state',
  hostkey: 'hostkey://challenge',
  sftpProgress: 'sftp://progress',
  sftpChanged: 'sftp://changed',
  tunnelState: 'tunnel://state',
  metrics: 'metrics://sample',
  appError: 'app://error',
  // Windows 11 贴靠布局：原生覆盖层独占最大化按钮的鼠标，hover/press 由后端转发
  snapHover: 'snap://hover',
  snapPress: 'snap://press',
  // 局域网对等层
  peerDiscovered: 'peer://discovered',
  peerPairing: 'peer://pairing',
  peerInbox: 'peer://inbox',
  peerControlRequest: 'peer://control-request',
  peerSession: 'peer://session',
  peerDenied: 'peer://denied',
  peerScreenRequest: 'peer://screen-request',
  peerScreenSession: 'peer://screen-session',
  peerMicRequest: 'peer://mic-request',
  peerMicState: 'peer://mic-state',
  peerInputStats: 'peer://input-stats',
  peerClipboard: 'peer://clipboard',
} as const;
