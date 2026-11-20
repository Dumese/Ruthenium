// Windows 路径工具：私钥路径的展示压缩与选择器默认目录推导。
// 纯函数，便于 vitest 覆盖；不依赖 Tauri API。

/** 去掉 Windows canonicalize 产生的 `\\?\` / `\\?\UNC\` 长路径前缀。
 *  UNC 路径 canonicalize 后是 `\\?\UNC\srv\share\...`，直接 slice(4) 会得到
 *  `UNC\srv\share\...`（非法相对路径）；必须还原成 `\\srv\share\...`。 */
export function stripLongPrefix(p: string): string {
  if (p.startsWith('\\\\?\\UNC\\')) return '\\\\' + p.slice(8);
  return p.startsWith('\\\\?\\') ? p.slice(4) : p;
}

/**
 * 把位于用户主目录下的绝对路径压成 `~\...` 形式，让 hosts.toml 可跨机器迁移
 * （本地优先：单一人类可读、可复制迁移的配置文件）。不在主目录下则原样返回。
 */
export function compactHomePath(absPath: string, home: string | null): string {
  const p = stripLongPrefix(absPath).replace(/\//g, '\\');
  if (!home) return p;
  const h = stripLongPrefix(home).replace(/\//g, '\\').replace(/\\+$/, '');
  if (!h) return p;
  const lowP = p.toLowerCase();
  const lowH = h.toLowerCase();
  if (lowP === lowH) return '~';
  if (lowP.startsWith(lowH + '\\')) return '~\\' + p.slice(h.length + 1);
  return p;
}

/** 从 keyRef 推导文件选择器的默认起始目录：`~\.ssh\id_rsa` → `~\.ssh`；绝对路径取父目录。 */
export function defaultKeyDir(keyRef: string | undefined): string | undefined {
  if (!keyRef) return '~\\.ssh';
  const p = keyRef.replace(/\//g, '\\');
  const i = p.lastIndexOf('\\');
  if (i <= 0) return p.startsWith('~') ? '~\\.ssh' : undefined;
  return p.slice(0, i);
}

/**
 * 「复制路径」按平台格式化：本地 `C:\Users\...`、远端 `/home/...`。
 * 本地根是盘符列表（name 自带 `\`），远端根是 `/`；两套逻辑不共用一套拼接。
 */
export function formatSidePath(side: 'local' | 'remote', base: string, name: string): string {
  if (side === 'local') {
    if (!base) return name;
    return (base.endsWith('\\') ? base : base + '\\') + name;
  }
  if (!name) return base || '/';
  return (base === '/' ? '' : base) + '/' + name;
}
