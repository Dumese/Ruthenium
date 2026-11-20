// 命令面板模糊匹配：子序列匹配 + 简单打分，纯函数可测
export interface PalItem {
  g: string;            // 分组：主机 / 片段 / 动作
  i: string;            // 图标名
  t: string;            // 标题
  s: string;            // 副标题
  act: string;          // 动作名
  arg?: string;
  /** 标题/副标题来自用户数据（主机名、分组名、片段名、命令原文），不是界面文案。
   *  渲染时落成 `data-user`，i18n 残检据此跳过——否则用户自己起的中文名会被
   *  误报成"英文界面中文残留"（而动作项副标题仍受检，真实泄漏不会被漏掉）。 */
  u?: boolean;
}

/** 连续命中加分、词首命中加分，用于排序 */
export function fuzzyScore(text: string, query: string): number {
  const s = query.toLowerCase().trim();
  if (!s) return 0;
  const t = text.toLowerCase();
  let score = 0, ti = 0;
  for (const ch of s) {
    if (ch === ' ') continue;
    const idx = t.indexOf(ch, ti);
    if (idx < 0) return -1;
    if (idx === ti) score += 3;          // 连续命中
    if (idx === 0 || /[\s@.\-_/]/.test(t[idx - 1] ?? '')) score += 2; // 词首
    score += 1;
    ti = idx + 1;
  }
  return score - t.length * 0.01;
}

export function filterPal(items: PalItem[], query: string, limit = 40): PalItem[] {
  if (!query.trim()) return items.slice(0, limit);
  return items
    .map((it) => {
      const combined = fuzzyScore(it.t + ' ' + it.s, query);
      const title = fuzzyScore(it.t, query);
      return { it, sc: Math.max(combined, title), matched: combined !== -1 || title !== -1 };
    })
    // 不能用 sc >= 0 过滤：长文本的合法匹配分数可能被长度惩罚压成负数。
    .filter((x) => x.matched)
    .sort((a, b) => b.sc - a.sc)
    .slice(0, limit)
    .map((x) => x.it);
}
