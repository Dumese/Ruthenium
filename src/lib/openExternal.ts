// 打开外部目标（URL 点击、日志文件夹）：
// - Tauri 下走 opener 插件（openUrl / openPath），由系统浏览器 / 资源管理器接管；
// - 纯浏览器 / mock 环境降级 window.open；本地目录没有可降级目标，返回 false 让调用方给提示。
// 只放行 http(s)（WebLinksAddon 默认也只会给 http(s)），不做任何出站探测。
import { isTauri } from '../ipc/env';

export async function openExternal(url: string): Promise<boolean> {
  const u = String(url ?? '').trim();
  if (!/^https?:\/\//i.test(u)) return false;
  if (isTauri) {
    try {
      const { openUrl } = await import('@tauri-apps/plugin-opener');
      await openUrl(u);
      return true;
    } catch { /* 插件不可用：回退 window.open（浏览器预览 / mock） */ }
  }
  try {
    // window.open 被拦截时返回 null 而不是抛错——如实报告失败，
    // 调用方据此提示（返回 true 会让“没反应”无从排查）。
    const w = window.open(u, '_blank', 'noopener,noreferrer');
    return !!w;
  } catch {
    return false;
  }
}

export async function openDirectory(path: string): Promise<boolean> {
  const p = String(path ?? '').trim();
  if (!p) return false;
  if (!isTauri) return false;
  try {
    const { openPath } = await import('@tauri-apps/plugin-opener');
    await openPath(p);
    return true;
  } catch {
    return false;
  }
}
