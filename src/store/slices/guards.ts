// 跨 slice 共享的模块级可变状态与纯函数。
//
// 为什么集中：这些 Set/Map 是连接闸门的隐式契约——connect 写入、closeSession/onConnState
// 消费、__resetSessionGuards 在测试里统一清空。分散到各 slice 文件会让「谁拥有这份状态」
// 变得不可见（import 双向、清理遗漏），集中一处 + 显式导出是最诚实的组织方式。
import { uid } from '../../lib/ids';
import { selectSftpSessionId } from '../selectors';
import type { FormDraft, Host, Session, SessionStatus, Transfer } from '../../types/models';
import { normalizePort } from '../../lib/ports';
import { useAppStore } from '../useAppStore';

/** 表单端口 → 合法端口；`null` 表示非法，调用方必须提示并中止保存。
 *  不能直接用 `Number(f.port)`：空串会得到 0，而 0 是「本地终端主机」哨兵，
 *  于是"把端口框清空"会把一台 SSH 主机静默变成本地终端（命令全部打回本机）。
 *  只有编辑一台**本来就是 port=0** 的主机时才允许继续保留 0。 */
export function resolveFormPort(f: FormDraft): number | null {
  const editing = f.id ? useAppStore.getState().hosts.find((h) => h.id === f.id) : undefined;
  return normalizePort(f.port, { allowZeroPort: editing?.port === 0 });
}

/** 密码弹窗的重试计数：同一主机在一次连接链路里最多重开一次 */
export const pwRetries = new Map<string, number>();
/** hostkey 挑战发生在哪个 kind 上：确认指纹后要按同一 kind 重新进闸门 */
export const pendingKinds = new Map<string, 'term' | 'sftp'>();
/** 自动重连计时器：sessionId → 1s 倒计时 interval；不放进 store，卸载 / 关标签时清理 */
export const retryTimers = new Map<string, ReturnType<typeof setInterval>>();
/** 最近一次重连失败的 error code：自动重连据此判断「认证失败不重试」 */
export const reconnectFailCode = new Map<string, string | undefined>();
/** 入队前先到的首帧进度（后端可能先于 transferId 返回发事件） */
export const pendingProgress = new Map<string, { transferId: string; bytesDone: number; bytesTotal: number; state: Transfer['state'] }>();
/** 首帧进度的淘汰定时器：同 id 后续帧刷新时重置（与 earlyConnStates 同型） */
export const pendingProgressTimers = new Map<string, ReturnType<typeof setTimeout>>();
/** 点连接后先出的临时标签；用户取消时记下，invoke 返回后立即关闭真实会话 */
export const cancelledConnects = new Set<string>();
/** connect() 返回前就到达的 conn://state（sessionId 尚未映射到 store），落地时回填 */
export const earlyConnStates = new Map<string, SessionStatus>();
/** init 静默建立的标签不弹 Toast */
export const quietSessions = new Set<string>();
/** reconnect 成功后用「已重新连接」文案 */
export const reconnectingSessions = new Set<string>();
/** 自动重连在途标记：防止「立即重连」在上一轮 await 期间重入同一会话（并发两次 reconnect） */
export const autoReconnecting = new Set<string>();
/** 自动重连轮次：调度/取消时自增。在途的 reconnect await 返回后发现代数变了，
 *  说明用户已取消或又排了新的一轮，必须放弃本轮收尾（否则会把已取消的会话
 *  重新标成重试中，或再排一次倒计时）。 */
export const retryGen = new Map<string, number>();
export function bumpRetryGen(id: string): number {
  const n = (retryGen.get(id) ?? 0) + 1;
  retryGen.set(id, n);
  return n;
}

let toastTimer: ReturnType<typeof setTimeout> | null = null;
export function setToastTimer(t: ReturnType<typeof setTimeout> | null): void { toastTimer = t; }
export function getToastTimer(): ReturnType<typeof setTimeout> | null { return toastTimer; }

let activeBeforeX: string | null = null;
export function setActiveBeforeX(v: string | null): void { activeBeforeX = v; }
export function getActiveBeforeX(): string | null { return activeBeforeX; }

/** 应用内剪贴板缓冲：粘贴优先读它，避免浏览器剪贴板权限导致异步挂起 */
let clipBuf = '';
export function setClipBuf(v: string): void { clipBuf = v; }
export function getClipBuf(): string { return clipBuf; }

let initStarted = false;
export function markInitStarted(): boolean {
  if (initStarted) return true;
  initStarted = true;
  return false;
}

export function clearRetryTimer(id: string): void {
  const t = retryTimers.get(id);
  if (t) { clearInterval(t); retryTimers.delete(id); }
}

export const PENDING_PROGRESS_TTL = 10_000;

/** 首帧进度的淘汰：同 id 后续帧刷新时重置；已消费或状态已推进则不动 */
export function schedulePendingProgressDrop(transferId: string, observedState: Transfer['state']): void {
  const old = pendingProgressTimers.get(transferId);
  if (old) clearTimeout(old);
  pendingProgressTimers.set(transferId, setTimeout(() => {
    pendingProgressTimers.delete(transferId);
    const cur = pendingProgress.get(transferId);
    // 已入队（被消费删除）或状态已推进则不动；仍是暂存的首帧才淘汰
    if (cur && cur.state === observedState) pendingProgress.delete(transferId);
  }, PENDING_PROGRESS_TTL));
}

/** 后端认证失败 detail 的分支（按文案分支，不新增错误码） */
export function authFailure(detail: string | undefined): 'missing' | 'rejected' | 'nocred' | null {
  const d = String(detail ?? '');
  if (!d) return null;
  if (/未保存密码|未设置密码|no password|password not saved/i.test(d)) return 'missing';
  if (/密码被拒绝|密码.*拒绝|password reject/i.test(d)) return 'rejected';
  if (/未找到可用凭据|no usable credential/i.test(d)) return 'nocred';
  return null;
}

/** 正式主机 + 快速连接的临时主机统一查找 */
export function findHostIn(s: { hosts: Host[]; transientHosts: Host[] }, id: string): Host | undefined {
  return s.hosts.find((x) => x.id === id) ?? s.transientHosts.find((x) => x.id === id);
}
export function isTransient(s: { hosts: Host[] }, id: string): boolean {
  return !s.hosts.some((x) => x.id === id);
}

/** 会话是否记录日志：后端生效条件是 Host.log || Preferences.logSessions，
 *  本地终端没有主机配置，只看全局默认。连接时按此快照进 Session.log。 */
export function sessionLogging(s: { hosts: Host[]; transientHosts: Host[]; logSessions: boolean }, hostId: string | undefined): boolean {
  const h = hostId ? findHostIn(s, hostId) : undefined;
  return h ? (!!h.log || !!s.logSessions) : !!s.logSessions;
}

/** SFTP 操作使用的会话对象：解析规则统一在 selectors.selectSftpSessionId（活动会话优先，与渲染层一致） */
export function sftpSession(s: { sessions: Session[]; active: string | null }): Session | undefined {
  const id = selectSftpSessionId(s);
  return id ? s.sessions.find((x) => x.id === id) : undefined;
}

export function pendingId(): string { return 'pending-' + uid(); }

// 本地栏 Windows 路径拼接 / 远程栏 POSIX 拼接（两套路径逻辑分离）
export const LOCAL_ROOT = '';
export function joinLocal(p: string, name: string): string {
  if (!p) return name.endsWith('\\') ? name : name + '\\';
  return p.endsWith('\\') ? p + name : p + '\\' + name;
}
export function joinRemote(p: string, name: string): string { return (p === '/' ? '' : p) + '/' + name; }

/** 表单 → Host（不包含密码；password/agent 认证不留无用 key_ref）。
 *  `port` 由调用方先经 `resolveFormPort` 校验（非法值要在保存前提示，不能静默落盘）。 */
export function hostFromForm(f: FormDraft, defaultGroup: string, port: number): Host {
  const auth = f.auth ?? 'key';
  return {
    id: f.id ?? uid(), name: String(f.name ?? ''), addr: f.addr || f.name || '', user: f.user || 'root',
    port, group: f.group || defaultGroup,
    tag: (f.tag as Host['tag']) || 'dev',
    auth,
    keyRef: auth === 'key' ? (f.keyRef || '~\\.ssh\\id_ed25519') : '',
    known: !!f.known, agent: !!f.agent, note: f.note ?? '',
    initCommand: String(f.initCommand ?? ''),
    log: !!f.log,
  };
}
