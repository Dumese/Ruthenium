// 终端写入的「单帧字节预算」自适应策略（纯函数，便于单测）：
// 预算过大 → 一次 term.write 堵住主线程；预算过小 → xterm 多解析/多渲染几次。
// 由 useXterm.ts 的 rAF 写入泵调用：每帧按预算取字节，再用量测耗时调整下一帧预算。

/** 单帧交给 xterm 解析的字节数上限（正常机）；低配机按量测耗时自动收紧。 */
export const MAX_BYTES_PER_FRAME = 256 * 1024;
/** 单帧字节数下限：再拆小只会让 xterm 多渲染几次，总耗时反而上升
 *  （16KB 下限时 128KB 输出的主线程耗时涨到 5 倍）。 */
export const MIN_BYTES_PER_FRAME = 64 * 1024;
/** 单帧写入耗时目标：超过它就减半下一帧字节数，避免一次大 write 把主线程堵住。 */
export const WRITE_BUDGET_MS = 12;

const clampBudget = (v: number): number => {
  // 先挡掉 NaN / ±Infinity：Math.max/min 会把它继续传播出去，最终写进预算就是灾难。
  if (!Number.isFinite(v)) return MIN_BYTES_PER_FRAME;
  return Math.min(MAX_BYTES_PER_FRAME, Math.max(MIN_BYTES_PER_FRAME, Math.floor(v)));
};

/**
 * 由「当前预算 + 本帧实际消费字节 + 本帧量测耗时」推导下一帧预算。
 *
 * 必须用**已消费字节**做算术，不能用循环结束后的**剩余**预算
 * （`frameBudget = budget * 2`）：本帧恰好把预算用尽时 `budget === 0`，
 * 下一帧预算被写成 0；而预算为 0 时既不会取出任何字节、也不会再进入调整分支，
 * 于是写入队列永久停滞（终端冻结）且 rAF 空转。
 * 所有出口强制夹在 [MIN, MAX]，从类型层面杜绝 0/nan/负数。
 *
 * @param current  本帧开始时的预算（字节）
 * @param consumed 本帧实际交给 xterm 的字节数（> 0）
 * @param costMs   本帧 term.write() 的量测耗时（毫秒）
 */
export function nextFrameBudget(current: number, consumed: number, costMs: number): number {
  // 异常输入（NaN / 负数 / 非有限）视为「无信息」，保持当前预算的合法值。
  const spent = Number.isFinite(consumed) && consumed > 0 ? consumed : MIN_BYTES_PER_FRAME;
  if (!Number.isFinite(costMs) || costMs < 0) return clampBudget(current);
  // 太慢：按本帧实际消费量减半（AIMD 的乘性下降）。
  if (costMs > WRITE_BUDGET_MS) return clampBudget(spent / 2);
  // 足够快：翻倍（但绝不低于 MIN，也绝不因「队列本来就小」而收缩当前上限）。
  if (costMs < WRITE_BUDGET_MS / 3) return clampBudget(Math.max(spent, current) * 2);
  // 落在目标区间：保持不动。
  return clampBudget(current);
}
