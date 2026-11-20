// 系统原生文件/目录选择对话框（Tauri dialog 插件）。浏览器模式下返回 null，
// 由调用方决定降级行为（私钥字段在浏览器里允许手动输入，桌面版必须走选择器）。
import { isTauri } from '../ipc/env';
import { pickBackupOpenPath, pickBackupSavePath, pickFolder } from '../ipc/commands';
import type { PickedPath } from '../types/ipc';
import { compactHomePath, defaultKeyDir } from './paths';

/** 用户主目录（用于把绝对路径压成 `~\...`）；取不到返回 null。 */
async function homeDir(): Promise<string | null> {
  if (!isTauri) return null;
  try {
    const mod = await import('@tauri-apps/api/path');
    return await mod.homeDir();
  } catch {
    return null;
  }
}

/**
 * 选择 SSH 私钥文件。返回可直接存进 key_ref 的值（主目录下压成 `~\...`）。
 * - 用户取消 / 非桌面环境：返回 null；
 * - 对话框打不开（权限缺失等）：**抛错**（不能把出错和取消都静默成 null，
 *   否则桌面端按钮点了没反应且无任何提示；与 pickSavePath 同一语义）。
 */
export async function pickPrivateKey(current?: string): Promise<string | null> {
  if (!isTauri) return null;
  let selected: string | null = null;
  try {
    const { open } = await import('@tauri-apps/plugin-dialog');
    const home = await homeDir();
    // 系统对话框不展开 `~`：把默认目录解析成绝对路径（优先当前 keyRef 所在目录）
    const startDir = defaultKeyDir(current);
    const defaultPath =
      startDir && startDir.startsWith('~')
        ? home
          ? joinWin(home, startDir.replace(/^~\\?/, ''))
          : undefined
        : startDir;
    const picked = await open({
      multiple: false,
      directory: false,
      // 不设 filters：私钥常无扩展名（id_rsa / id_ed25519），加过滤器会把它们藏起来
      ...(defaultPath ? { defaultPath } : {}),
    });
    selected = typeof picked === 'string' ? picked : null;
  } catch (e) {
    // 对话框不可用（权限缺失等）：抛给调用方明确提示
    throw new Error(`打开私钥选择器失败：${(e as Error)?.message ?? e}`);
  }
  if (selected === null) return null; // 用户取消
  const home = await homeDir();
  return compactHomePath(selected, home);
}

/** 拼接 Windows 目录与相对片段（避免重复/缺失分隔符）。 */
function joinWin(dir: string, rest: string): string {
  const d = dir.replace(/[\\/]+$/, '');
  const r = rest.replace(/^[\\/]+/, '');
  return r ? `${d}\\${r}` : d;
}

/** 选择保存路径（备份导出用）。
 *
 * 走**后端**命令而不是 JS 插件：后端以主窗口为 parent 创建对话框，保证它一定位于
 * 应用窗口之前（JS 插件的对话框没有 owner，某些窗口状态下会被主窗口盖住，用户会
 * 以为按钮没反应）。用户取消返回 null；对话框打不开会抛错，由调用方提示 —— 不能
 * 把「出错」和「取消」都静默成 null，否则功能看起来像没实现。
 *
 * 返回 `{token, path}`——token 是后续 export_backup 的一次性凭据，
 * path 仅用于展示（toast「已导出到 …」）。
 */
export async function pickSavePath(defaultName: string, title = '导出配置备份'): Promise<PickedPath | null> {
  if (!isTauri) return null;
  return pickBackupSavePath(title, defaultName);
}

/** 打开 JSON 备份文件（导入用）。语义同 [`pickSavePath`]（token 供 import_backup 消费）。 */
export async function pickBackupFile(title = '选择配置备份文件'): Promise<PickedPath | null> {
  if (!isTauri) return null;
  return pickBackupOpenPath(title);
}
/** 选择目录（局域网共享根）：原生资源管理器选择器；取消返回 null，失败抛错。 */
export async function pickShareFolder(current?: string, title = '选择共享目录'): Promise<string | null> {
  try {
    return await pickFolder(title, current);
  } catch (e) {
    throw new Error(`打开目录选择器失败：${(e as Error)?.message ?? e}`);
  }
}