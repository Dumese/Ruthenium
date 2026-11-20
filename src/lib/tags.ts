// 主机标签（hosts.toml 的 host.tag）。
//
// 约定：选项为 **开发 / 测试 / 生产 / 堡垒机**。
// - 落盘值与 Chip 配色用稳定 ASCII id（dev / test / prod / bastion），
//   显示名走 i18n（`htag.*`），换语言不需要改数据。
// - 旧配置里可能用 `stage` 表示「测试」，读盘时统一归一化成 `test`。
// - 无法识别的值（手改/导入的数据）保持原样显示，不强行改写成默认值，
//   避免悄悄丢信息；主机表单会把它作为额外一项列出来。
import { T } from '../i18n';

export const HOST_TAGS = ['dev', 'test', 'prod', 'bastion'] as const;
export type HostTagId = (typeof HOST_TAGS)[number];

/** 旧标签值 → 新值（读取旧配置时迁移）。 */
const LEGACY_TAG: Record<string, HostTagId> = { stage: 'test' };

/** 归一化标签：空值回落 dev，旧值 stage→test，其余原样返回。 */
export function normalizeHostTag(tag: string | undefined | null): string {
  const t = (tag ?? '').trim();
  if (!t) return HOST_TAGS[0];
  return LEGACY_TAG[t] ?? t;
}

/** 标签显示名：已知 id 走 i18n；未知值原样显示（不把 i18n key 暴露给用户）。 */
export function tagLabel(tag: string | undefined | null): string {
  const t = normalizeHostTag(tag);
  return (HOST_TAGS as readonly string[]).includes(t) ? T(`htag.${t}`) : t;
}

/** 批量归一化主机标签（读盘 / 导入备份后调用）。 */
export function normalizeHostTags<T extends { tag?: string }>(hosts: T[]): T[] {
  return hosts.map((h) => ({ ...h, tag: normalizeHostTag(h.tag) }));
}
