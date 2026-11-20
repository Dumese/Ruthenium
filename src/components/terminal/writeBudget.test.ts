// 单帧预算无论如何都必须落在 [MIN, MAX]，永远不能变成 0。
// 用剩余预算做乘 2 时，在「预算恰好用尽 + 写入很快」时会算出 0，
// 导致后续帧不再取字节、不再更新预算，终端写入队列永久停滞。
import { describe, expect, it } from 'vitest';
import {
  MAX_BYTES_PER_FRAME,
  MIN_BYTES_PER_FRAME,
  WRITE_BUDGET_MS,
  nextFrameBudget,
} from './writeBudget';

describe('nextFrameBudget', () => {
  it('预算恰好用尽且写入很快时不得归零（原始触发条件）', () => {
    // 预算用剩余预算做乘 2 时：budget = 256K - 256K = 0 → 0 * 2 = 0 → 永久 0
    const next = nextFrameBudget(MAX_BYTES_PER_FRAME, MAX_BYTES_PER_FRAME, 1);
    expect(next).toBeGreaterThanOrEqual(MIN_BYTES_PER_FRAME);
    expect(next).toBeLessThanOrEqual(MAX_BYTES_PER_FRAME);
  });

  it('下限档（64K）用尽且很快 → 翻倍而不是归零', () => {
    expect(nextFrameBudget(MIN_BYTES_PER_FRAME, MIN_BYTES_PER_FRAME, 0.5)).toBe(
      MIN_BYTES_PER_FRAME * 2,
    );
  });

  it('写入过慢 → 按实际消费量减半，且不低于 MIN', () => {
    expect(nextFrameBudget(MAX_BYTES_PER_FRAME, MAX_BYTES_PER_FRAME, WRITE_BUDGET_MS + 1)).toBe(
      MAX_BYTES_PER_FRAME / 2,
    );
    // 只写了 8K 却超时：减半后会低于下限 → 夹到 MIN
    expect(nextFrameBudget(MAX_BYTES_PER_FRAME, 8 * 1024, 50)).toBe(MIN_BYTES_PER_FRAME);
  });

  it('耗时落在目标区间 → 保持当前预算', () => {
    const mid = WRITE_BUDGET_MS / 2;
    expect(nextFrameBudget(128 * 1024, 128 * 1024, mid)).toBe(128 * 1024);
  });

  it('字节很少但队列本来很大 → 不因「本帧写得少」而收缩上限', () => {
    expect(nextFrameBudget(MAX_BYTES_PER_FRAME, 8 * 1024, 1)).toBe(MAX_BYTES_PER_FRAME);
  });

  it('非法输入（NaN / 负数 / 非有限）也只会得到合法预算', () => {
    for (const [cur, used, cost] of [
      [0, 0, 0],
      [Number.NaN, Number.NaN, Number.NaN],
      [-1, -1, -1],
      [Number.POSITIVE_INFINITY, Number.POSITIVE_INFINITY, Number.POSITIVE_INFINITY],
      [MAX_BYTES_PER_FRAME, 0, Number.NaN],
    ] as const) {
      const next = nextFrameBudget(cur, used, cost);
      expect(Number.isFinite(next)).toBe(true);
      expect(next).toBeGreaterThanOrEqual(MIN_BYTES_PER_FRAME);
      expect(next).toBeLessThanOrEqual(MAX_BYTES_PER_FRAME);
    }
  });

  it('全参数网格：结果恒在 [MIN, MAX]（穷举边界组合，防回归）', () => {
    const curs = [MIN_BYTES_PER_FRAME, MAX_BYTES_PER_FRAME, 0, 1, 1e9];
    const used = [1, MIN_BYTES_PER_FRAME, MAX_BYTES_PER_FRAME, MAX_BYTES_PER_FRAME * 2];
    const costs = [0, 1, WRITE_BUDGET_MS / 3, WRITE_BUDGET_MS / 3 + 0.001, WRITE_BUDGET_MS, 1000];
    for (const c of curs) {
      for (const u of used) {
        for (const k of costs) {
          const next = nextFrameBudget(c, u, k);
          expect(next).toBeGreaterThanOrEqual(MIN_BYTES_PER_FRAME);
          expect(next).toBeLessThanOrEqual(MAX_BYTES_PER_FRAME);
        }
      }
    }
  });
});

/**
 * 复刻 useXterm.flushWrites 的取字节/更新预算流程（不依赖 xterm），
 * 用「每帧预算恰好被用满 + 写入极快」这一会让预算归零的场景，验证队列一定排空。
 */
function drainFrames(
  totalBytes: number,
  costOf: (writtenThisFrame: number) => number,
  maxFrames: number,
): { remaining: number; frames: number; minBudget: number } {
  let budget = MAX_BYTES_PER_FRAME;
  let remaining = totalBytes;
  let minBudget = budget;
  let frames = 0;
  while (remaining > 0 && frames < maxFrames) {
    if (!(budget >= MIN_BYTES_PER_FRAME)) budget = MIN_BYTES_PER_FRAME;
    const take = Math.min(remaining, budget);
    remaining -= take;
    budget = nextFrameBudget(budget, take, costOf(take));
    minBudget = Math.min(minBudget, budget);
    frames += 1;
  }
  return { remaining, frames, minBudget };
}

describe('flushWrites 预算循环（回归）', () => {
  it('2 MiB 大输出在有限帧内排空（预算归零后永不排空）', () => {
    // 每次写入都极快 → cost < 4ms 且预算用尽时若用剩余预算做乘 2，frameBudget = 0 → 永久卡死
    const r = drainFrames(2 * 1024 * 1024, () => 0.2, 200);
    expect(r.remaining).toBe(0);
    expect(r.minBudget).toBeGreaterThanOrEqual(MIN_BYTES_PER_FRAME);
  });

  it('慢写入/快写入交替也不会把队列卡住', () => {
    let i = 0;
    const r = drainFrames(4 * 1024 * 1024, () => (i++ % 3 === 0 ? 40 : 0.1), 400);
    expect(r.remaining).toBe(0);
    expect(r.minBudget).toBeGreaterThanOrEqual(MIN_BYTES_PER_FRAME);
  });

  it('每帧只前进 1 字节的病态耗时下仍然会推进（不会 0 预算死循环）', () => {
    const r = drainFrames(4096, () => 0.01, 100);
    expect(r.remaining).toBe(0);
  });
});
