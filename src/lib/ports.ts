// 端口规范化（主机表单 / 临时主机 / 备份导入等入口共用）。
//
// `Number('') === 0`，而 port=0 在本项目里是「本地终端主机」的哨兵值
// （后端 commands/hosts.rs 注释、前端 requestConnect 的 host.port === 0 分支）。
// 端口框清空后若按 0 保存，主机会被静默改成"本地终端"，
// 用户以为在远端执行命令、实际全部落在本机。这里把「空串 = 用 SSH 默认端口」
// 与「0 = 本地终端哨兵」明确区分开，并对非法输入返回 null 交给调用方提示。

/** SSH 默认端口（与后端 models.rs 的 default_ssh_port 保持一致）。 */
export const DEFAULT_SSH_PORT = 22;
/** 最大合法端口（u16 上界）。 */
export const MAX_PORT = 65535;

export interface PortNormalizeOptions {
  /** 允许保留 0（本地终端哨兵）。只应在"编辑一台本来就是 port=0 的主机"时传 true。 */
  allowZeroPort?: boolean;
}

/**
 * 把表单里的端口输入规范化为合法端口号。
 *
 * - 空串 / 纯空白 → {@link DEFAULT_SSH_PORT}（等价于"用默认端口"）；
 * - 纯十进制整数且在 1..=65535 → 该整数；
 * - `0`：仅在 `allowZeroPort` 时保留（本地终端哨兵），否则视为非法；
 * - 其余（负数、越界、`22abc`、`1e3`、`0x16`、NaN、非数字）→ `null`。
 */
export function normalizePort(input: unknown, opts: PortNormalizeOptions = {}): number | null {
  if (typeof input === 'number' && !Number.isFinite(input)) return null;
  const raw = String(input ?? '').trim();
  if (raw === '') return DEFAULT_SSH_PORT;
  // 只接受纯十进制整数：Number() 会把 '1e3' / '0x16' / '22abc' 宽松解析成数字。
  if (!/^\d+$/.test(raw)) return null;
  const n = Number(raw);
  if (!Number.isSafeInteger(n)) return null;
  if (n === 0) return opts.allowZeroPort ? 0 : null;
  if (n < 1 || n > MAX_PORT) return null;
  return n;
}
