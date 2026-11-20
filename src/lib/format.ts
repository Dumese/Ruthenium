// 通用格式化工具
export function fmtSize(n: number | null | undefined): string {
  if (n == null) return '—';
  if (n < 1024) return n + ' B';
  if (n < 1048576) return (n / 1024).toFixed(1) + ' KB';
  if (n < 1073741824) return (n / 1048576).toFixed(1) + ' MB';
  return (n / 1073741824).toFixed(1) + ' GB';
}

/** 两个时刻的时长描述，如「42 天」或「3:11:08」 */
export function duration(fromMs: number, toMs: number = Date.now()): string {
  const s = Math.max(0, Math.floor((toMs - fromMs) / 1000));
  if (s < 60) return s + ' 秒';
  if (s < 3600) return Math.floor(s / 60) + ' 分钟';
  if (s < 86400) {
    const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60);
    return h + ':' + String(m).padStart(2, '0');
  }
  return Math.floor(s / 86400) + ' 天';
}

export function hhmm(ts: number = Date.now()): string {
  const d = new Date(ts);
  return String(d.getHours()).padStart(2, '0') + ':' + String(d.getMinutes()).padStart(2, '0');
}

/** POSIX 权限位 → `drwxr-xr-x`（10 字符）；无权限信息（Windows 本地）返回 `—`。 */
export function fmtMode(mode: number | null | undefined, isDir = false): string {
  if (mode == null || !Number.isFinite(mode)) return '—';
  const rwx = (bits: number) => ((bits & 4) ? 'r' : '-') + ((bits & 2) ? 'w' : '-') + ((bits & 1) ? 'x' : '-');
  const type = isDir || (mode & 0o170000) === 0o040000 ? 'd' : (mode & 0o170000) === 0o120000 ? 'l' : '-';
  return type + rwx((mode >> 6) & 7) + rwx((mode >> 3) & 7) + rwx(mode & 7);
}
