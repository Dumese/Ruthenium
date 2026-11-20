// SFTP slice：双栏路径状态、目录导航、文件操作、传输队列、每主机路径记忆。
import { T, terr } from '../../i18n';
import * as api from '../../ipc/commands';
import type { SliceCreator } from './shared';
import {
  sftpSession, joinLocal, joinRemote, LOCAL_ROOT,
  pendingProgress, pendingProgressTimers,
} from './guards';

export interface SftpSlice {
  // SFTP（本地根为盘符列表：''）
  localPath: string; remotePath: string; selLocal: string | null; selRemote: string | null;
  /** 当前 remotePath 属于哪个主机（null = 尚未确定）。多主机 SFTP 并存时
   *  按它判断路径归属——切主机必须装载该主机的记忆路径，否则 A 的路径会串到 B。 */
  remotePathHostId: string | null;
  /** 各终端会话的当前工作目录（由 OSC 7 上报，SFTP 默认打开这里） */
  cwdBySession: Record<string, string>;
  /** 每台主机上次浏览的远端目录（再次打开 SFTP 时回到上次位置） */
  sftpPathByHost: Record<string, string>;
  sftpRev: number;

  // SFTP
  setSftpPath(side: 'local'|'remote', path: string): void;
  /** 终端通过 OSC 7 上报的 cwd */
  setSessionCwd(sessionId: string, path: string): void;
  /** 首次进入 SFTP 时决定远端起始目录：cwd → 上次路径 → 服务器 home */
  ensureRemotePath(): Promise<void>;
  selectFile(side: 'local'|'remote', name: string | null): void;
  openDir(side: 'local'|'remote', name: string): void;
  upDir(side: 'local'|'remote'): void;
  refreshSftp(quiet?: boolean): void;
  mkdirSave(): Promise<void>;
  /** 删除文件：target 来自右键菜单，保证删的是右键那一侧（不带则回退到当前选中） */
  deleteSelectedFile(target?: { side: 'local'|'remote'; name: string }): Promise<void>;
  doTransfer(dir: 'up'|'down', opts?: { name?: string; targetPath?: string }): Promise<void>;
  transferDrop(side: 'local'|'remote', name: string, targetPath?: string): Promise<void>;
  /** 从资源管理器拖入的本地文件批量上传（Tauri drag-drop 事件给出真实路径） */
  uploadDroppedFiles(paths: string[], targetDir?: string): Promise<void>;
  clearQueue(): void;
  cancelTransfer(id: string): Promise<void>;
}

export const createSftpSlice: SliceCreator<SftpSlice> = (set, get) => ({
  localPath: LOCAL_ROOT, remotePath: '', remotePathHostId: null,
  cwdBySession: {}, sftpPathByHost: {}, selLocal: null, selRemote: null, sftpRev: 0,

  setSftpPath(side, path) {
    if (side === 'local') { set({ localPath: path ?? LOCAL_ROOT, selLocal: null }); }
    else {
      const next = path || '/';
      // 记住每台主机最后浏览的目录（会话解析与渲染层同规则：活动会话优先）
      const sess = sftpSession(get());
      const hid = sess?.hostId;
      set({ remotePath: next, remotePathHostId: hid ?? null, selRemote: null });
      if (hid) set({ sftpPathByHost: { ...get().sftpPathByHost, [hid]: next } });
    }
    set({ sftpRev: get().sftpRev + 1 });
  },
  setSessionCwd(sessionId, path) {
    if (!path) return;
    const cur = get().cwdBySession;
    if (cur[sessionId] === path) return;
    set({ cwdBySession: { ...cur, [sessionId]: path } });
  },
  async ensureRemotePath() {
    const s = get();
    const sess = sftpSession(s);
    // 路径非空**且属于当前主机**才算已确定——“非空即早退”会让
    // 主机 A 的路径串到主机 B 的界面上（每主机记忆完全失效）。
    const owned = s.remotePath && (s.remotePathHostId === null || s.remotePathHostId === sess?.hostId);
    if (owned) return;
    if (!sess) return;
    const remembered = s.sftpPathByHost[sess.hostId];
    if (remembered) {
      set({ remotePath: remembered, remotePathHostId: sess.hostId, selRemote: null, sftpRev: s.sftpRev + 1 });
      return;
    }
    const cwd = s.cwdBySession[sess.id];
    if (cwd) {
      set({ remotePath: cwd, remotePathHostId: sess.hostId, selRemote: null, sftpRev: s.sftpRev + 1 });
      return;
    }
    try {
      const home = await api.sftpHome(sess.id);
      // await 期间用户可能切到另一台主机；迟到的 home 不能写到新界面上。
      const current = sftpSession(get());
      if (home && current?.id === sess.id) {
        set({ remotePath: home, remotePathHostId: sess.hostId, selRemote: null, sftpRev: get().sftpRev + 1 });
      }
    } catch {
      // 拿不到 home（连接刚断等）：保持空，UI 显示空目录提示，不弹错误
    }
  },
  selectFile(side, name) {
    // 两栏各自选中会让“工具栏删除”不知道目标；只保留最后点击的一侧。
    if (side === 'local') set({ selLocal: name, selRemote: null });
    else set({ selRemote: name, selLocal: null });
  },
  openDir(side, name) {
    if (side === 'local') get().setSftpPath('local', joinLocal(get().localPath, name));
    else get().setSftpPath('remote', joinRemote(get().remotePath, name));
  },
  upDir(side) {
    if (side === 'local') {
      const trimmed = get().localPath.replace(/[\\/]+$/, '');
      const idx = trimmed.lastIndexOf('\\');
      if (idx < 0) { get().setSftpPath('local', ''); return; }
      let parent = trimmed.slice(0, idx);
      // `C:` 在 Windows 上是“该盘当前目录”，不是根；上一级必须是 `C:\`。
      if (/^[A-Za-z]:$/.test(parent)) parent += '\\';
      get().setSftpPath('local', parent);
    } else {
      const a = get().remotePath.split('/').filter(Boolean); a.pop();
      get().setSftpPath('remote', '/' + a.join('/'));
    }
  },
  refreshSftp(quiet = false) {
    set({ sftpRev: get().sftpRev + 1 });
    if (!quiet) get().showToast(T('toast.refreshed'));
  },
  async mkdirSave() {
    const inp = get().input;
    if (!inp) return;
    const name = inp.value.trim();
    set({ overlay: null, input: null });
    if (!name) { get().showToast(T('toast.needFolderName')); return; }
    const side = inp.side;
    try {
      if (side === 'local') await api.localMkdir(get().localPath, name);
      else {
        const s = sftpSession(get());
        if (!s) throw { code: 'E_NOT_FOUND', detail: T('err.noSession') };
        await api.sftpMkdir(s.id, joinRemote(get().remotePath, name));
      }
      get().showToast(T('toast.folderCreated', { 0: name }));
      get().refreshSftp(true);
    } catch (e) {
      get().showToast(T('err.opFailed', { 0: T('mk.create'), 1: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail) }));
    }
  },
  async deleteSelectedFile(target) {
    // target 来自右键菜单：只删右键那一侧，不因为另一侧也有选中而删错
    const side = target?.side ?? (get().selRemote ? 'remote' : get().selLocal ? 'local' : null);
    if (!side) { get().showToast(T('toast.pickFile')); return; }
    const name = target?.name ?? (side === 'local' ? get().selLocal : get().selRemote);
    const full = side === 'local' ? joinLocal(get().localPath, name ?? '') : joinRemote(get().remotePath, name ?? '');
    try {
      if (side === 'local') await api.localDelete(full);
      else {
        const s = sftpSession(get());
        if (!s) throw { code: 'E_NOT_FOUND', detail: T('err.noSession') };
        await api.sftpDelete(s.id, full);
      }
      if (side === 'local') set({ selLocal: null }); else set({ selRemote: null });
      get().showToast(T('toast.deleted', { 0: name ?? '' }));
      get().refreshSftp(true);
    } catch (e) {
      get().showToast(T('err.opFailed', { 0: T('c.delete'), 1: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail) }));
    }
  },
  async doTransfer(dir, opts) {
    const s = get();
    const sess = sftpSession(s);
    if (!sess) { s.showToast(T('toast.noTarget')); return; }
    if (dir === 'up') {
      const name = opts?.name ?? s.selLocal;
      if (!name) { s.showToast(T('err.pickLeft')); return; }
      const localPath = joinLocal(s.localPath, name);
      const targetPath = opts?.targetPath ?? s.remotePath;
      try {
        const id = await api.sftpUpload(sess.id, localPath, targetPath);
        get().addTransfer({ id, name, dir: 'up', bytesDone: 0, bytesTotal: 1, state: 'running' });
        set({ selLocal: null });
      } catch (e) { s.showToast(T('err.opFailed', { 0: T('sftp.upShort'), 1: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail) })); }
    } else {
      const name = opts?.name ?? s.selRemote;
      if (!name) { s.showToast(T('err.pickRight')); return; }
      const remotePath = joinRemote(s.remotePath, name);
      const targetPath = opts?.targetPath ?? s.localPath;
      try {
        const id = await api.sftpDownload(sess.id, remotePath, targetPath);
        get().addTransfer({ id, name, dir: 'down', bytesDone: 0, bytesTotal: 1, state: 'running' });
        set({ selRemote: null });
      } catch (e) { s.showToast(T('err.opFailed', { 0: T('sftp.downShort'), 1: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail) })); }
    }
  },
  /** SFTP 原生拖放：side 是拖拽源；targetPath 是落点目录（缺省为对侧当前目录） */
  async transferDrop(side, name, targetPath) {
    await get().doTransfer(side === 'local' ? 'up' : 'down', { name, targetPath });
  },
  /**
   * 从资源管理器拖入文件直接上传：Tauri drag-drop 事件给出真实本地路径。
   * - 目标为远端当前目录（或拖拽落点的目录行），后端对已存在目录自动拼接源文件名；
   * - 文件夹无法判断直接上传会失败：用 local_list 试探（能列出即目录），跳过并提示；
   * - 逐个串行入队（保持传输队列顺序），完成后 toast 汇总；目录刷新由 sftp://changed 驱动。
   */
  async uploadDroppedFiles(paths, targetDir) {
    const s = get();
    const sess = sftpSession(s);
    if (!sess) { s.showToast(T('toast.noTarget')); return; }
    const files = paths.filter(Boolean);
    if (!files.length) return;
    const target = targetDir ?? get().remotePath;
    let queued = 0; let skipped = 0;
    for (const p of files) {
      // 目录试探：local_list 对目录成功、对文件报错（比上传到一半失败友好得多）
      let isDir = false;
      try { await api.localList(p); isDir = true; } catch { /* 文件：继续 */ }
      if (isDir) { skipped += 1; continue; }
      const name = p.split(/[\\/]/).filter(Boolean).pop() ?? p;
      try {
        const id = await api.sftpUpload(sess.id, p, target);
        get().addTransfer({ id, name, dir: 'up', bytesDone: 0, bytesTotal: 1, state: 'running' });
        queued += 1;
      } catch (e) {
        get().showToast(T('err.opFailed', {
          0: T('sftp.upShort'),
          1: `${name}: ${terr((e as { code?: string })?.code, (e as { detail?: string })?.detail)}`,
        }));
      }
    }
    if (queued) get().showToast(T('toast.extUploadQueued', { 0: queued }));
    if (skipped) get().showToast(T('toast.extUploadSkipped', { 0: skipped }));
  },
  clearQueue() {
    // 清空队列时一并丢掉首帧进度缓存，避免“已清空的传输”留下永久 Map 条目。
    pendingProgress.clear();
    pendingProgressTimers.forEach((t) => clearTimeout(t));
    pendingProgressTimers.clear();
    set({ transfers: [] });
  },
  async cancelTransfer(id) {
    // E_NOT_FOUND = 传输已结束/句柄已清理，本地同步为终态即可；
    // 其它失败（后端仍在传）不得假装“已取消”——如实提示并保持 running，
    // 否则后续进度事件会覆盖状态、误导用户。
    try {
      await api.sftpCancel(id);
      set({ transfers: get().transfers.map((t) => (t.id === id ? { ...t, state: 'cancelled' } : t)) });
    } catch (e) {
      if ((e as { code?: string })?.code === 'E_NOT_FOUND') {
        set({ transfers: get().transfers.map((t) => (t.id === id ? { ...t, state: 'cancelled' } : t)) });
        return;
      }
      get().showToast(T('err.opFailed', {
        0: T('sftp.cancelled'),
        1: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail),
      }));
    }
  },
});
