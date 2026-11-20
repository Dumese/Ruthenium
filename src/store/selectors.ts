// 派生选择器：activeSession / viewState / filteredHosts / selectSftpSessionId 等
import type { Host, Session, ViewState, Folder } from '../types/models';
import { useAppStore } from './useAppStore';

export function activeSession(s = useAppStore.getState()): Session | null {
  return s.sessions.find((x) => x.id === s.active) ?? null;
}

/** Stage 当前应渲染哪一种内容 */
export function viewState(s = useAppStore.getState()): ViewState {
  if (s.active === 'vaults') return 'vaults';
  if (s.active === 'snipsPage') return 'snipsPage';
  if (s.active === 'rulesPage') return 'rulesPage';
  if (s.active === 'sftpPage') return 'sftpPage';
  if (s.active === 'peers') return 'peers';
  const a = activeSession(s);
  if (!a) return 'empty';
  if (a.kind === 'screen') return 'screen';
  return a.kind === 'sftp' ? 'sftp' : 'terminal';
}

/**
 * SFTP 操作应使用哪个会话（多主机并存时防止「在 B 的界面上传、文件落到 A」）：
 * active 会话为 sftp / term → 用 active（与渲染层 FilePane / SftpView 同一优先级）；
 * 否则第一个 sftp 会话；否则第一个 term 会话；否则 null。
 * 渲染层与 store 的 6 处 SFTP 调用共用本函数，保证「看到的」与「操作到」的是同一条会话。
 */
export function selectSftpSessionId(s: { sessions: Session[]; active: string | null }): string | null {
  const a = s.sessions.find((x) => x.id === s.active);
  if (a && (a.kind === 'sftp' || a.kind === 'term')) return a.id;
  return s.sessions.find((x) => x.kind === 'sftp')?.id
    ?? s.sessions.find((x) => x.kind === 'term')?.id
    ?? null;
}

/** 分组来源：Config.folders（渲染层兜底用主机自带分组，不写死常量进契约） */
export function foldersOf(s = useAppStore.getState()): Folder[] {
  if (s.folders.length) return s.folders;
  const seen: string[] = [];
  s.hosts.forEach((h) => { if (!seen.includes(h.group)) seen.push(h.group); });
  return seen.map((name) => ({ name, color: '' }));
}

export function filterHosts(hosts: Host[], q: string): Host[] {
  const query = q.toLowerCase();
  if (!query) return hosts;
  return hosts.filter((x) => (x.name + x.addr + x.user + x.note).toLowerCase().includes(query));
}

/** 主机查找：正式主机 + 快速连接的临时主机 */
export function hostById(id: string, s = useAppStore.getState()): Host | null {
  return s.hosts.find((x) => x.id === id) ?? s.transientHosts.find((x) => x.id === id) ?? null;
}

export function hostName(id: string, s = useAppStore.getState()): string {
  return hostById(id, s)?.name ?? id;
}
