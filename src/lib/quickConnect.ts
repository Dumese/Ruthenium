// 快速连接语法解析：纯函数，便于 vitest 覆盖；只做语法与 Host 构造，不碰 IPC。
// 语法固定为 user@host[:port]：user 可省略，host 不含 @ / : / 空白，port 为 1–65535。
import type { Host } from '../types/models';

/** 语法正则（命名分组）：`^(?:(?<user>[^@]+)@)?(?<host>[^:@\s]+)(?::(?<port>\d+))?$` */
export const QUICK_CONNECT_RE = /^(?:(?<user>[^@]+)@)?(?<host>[^:@\s]+)(?::(?<port>\d+))?$/;

export interface QuickTarget { user: string; host: string; port: number }

/** 解析 `user@host[:port]`；非法输入返回 null（调用方给提示，不建会话） */
export function parseQuickConnect(input: string): QuickTarget | null {
  const raw = String(input ?? '').trim();
  if (!raw || raw.length > 255) return null;
  const m = QUICK_CONNECT_RE.exec(raw);
  const g = m?.groups;
  if (!g) return null;
  const port = g.port == null ? 22 : Number(g.port);
  if (!Number.isInteger(port) || port < 1 || port > 65535) return null;
  return { user: g.user ?? '', host: g.host, port };
}

/** 标签 / 主机名：有用户名时 `user@host`，否则 `host` */
export function quickTargetLabel(t: QuickTarget): string {
  return t.user ? `${t.user}@${t.host}` : t.host;
}

/**
 * 构造临时 Host：
 * - `id: 'quick-' + Date.now()`、`persist: false`（仅前端标记，不写 hosts.toml）
 * - 认证默认 agent、known=false（走统一 requestConnect 闸门，指纹照弹 / 密码照问）
 * - name 就是标签标题（`user@host`）
 */
export function buildQuickHost(t: QuickTarget, now: number = Date.now()): Host {
  return {
    id: 'quick-' + now,
    name: quickTargetLabel(t),
    addr: t.host,
    user: t.user || 'root',
    port: t.port,
    group: '',
    tag: 'dev',
    keyRef: '',
    auth: 'agent',
    known: false,
    agent: true,
    note: '',
    initCommand: '',
    log: false,
    persist: false,
  };
}
