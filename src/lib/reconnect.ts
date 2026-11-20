// 断线自动重连策略：固定指数退避 1s → 2s → 4s → 8s → 16s，最多 5 次。
// 策略不做配置项（克制）；纯函数便于单测覆盖阶梯与上限。
export const RECONNECT_DELAYS_MS = [1000, 2000, 4000, 8000, 16000] as const;
export const MAX_RECONNECT_ATTEMPTS = RECONNECT_DELAYS_MS.length;

/** 第 attempt 次（从 1 开始）重连前的等待毫秒数；超出上限时按最后一次的间隔 */
export function reconnectDelayMs(attempt: number): number {
  const i = Math.min(Math.max(Math.trunc(attempt) || 1, 1), MAX_RECONNECT_ATTEMPTS) - 1;
  return RECONNECT_DELAYS_MS[i];
}

/** 倒计时显示用的整秒数 */
export function reconnectSeconds(attempt: number): number {
  return Math.round(reconnectDelayMs(attempt) / 1000);
}

/** 是否还有下一次重试机会（attempt 是「已完成 / 正在等待」的次数） */
export function hasRetryLeft(attempt: number): boolean {
  return attempt < MAX_RECONNECT_ATTEMPTS;
}
