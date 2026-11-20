// 自动重连退避阶梯：固定 1/2/4/8/16s、最多 5 次，不读配置
import { describe, it, expect } from 'vitest';
import { reconnectDelayMs, reconnectSeconds, hasRetryLeft, MAX_RECONNECT_ATTEMPTS, RECONNECT_DELAYS_MS } from './reconnect';

describe('自动重连策略', () => {
  it('阶梯固定为 1s → 2s → 4s → 8s → 16s', () => {
    expect(RECONNECT_DELAYS_MS).toEqual([1000, 2000, 4000, 8000, 16000]);
    expect([1, 2, 3, 4, 5].map(reconnectDelayMs)).toEqual([1000, 2000, 4000, 8000, 16000]);
    expect([1, 2, 3, 4, 5].map(reconnectSeconds)).toEqual([1, 2, 4, 8, 16]);
  });
  it('最多 5 次', () => {
    expect(MAX_RECONNECT_ATTEMPTS).toBe(5);
    expect(hasRetryLeft(1)).toBe(true);
    expect(hasRetryLeft(4)).toBe(true);
    expect(hasRetryLeft(5)).toBe(false);
  });
  it('越界 attempt 夹回边界（不会返回 undefined/NaN）', () => {
    expect(reconnectDelayMs(0)).toBe(1000);
    expect(reconnectDelayMs(-3)).toBe(1000);
    expect(reconnectDelayMs(9)).toBe(16000);
    expect(reconnectDelayMs(Number.NaN)).toBe(1000);
  });
});
