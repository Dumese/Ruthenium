// 覆盖层 slice：Sheet / 命令面板 / 确认与输入弹窗 / 指纹弹窗挂载点 / 右键菜单动作。
import { T, terr } from '../../i18n';
import { formatSidePath } from '../../lib/paths';
import * as api from '../../ipc/commands';
import {
  getTermSelection, selectAllTerm, pasteToTerminal, clearTerminal, focusTerm,
} from '../../components/terminal/useXterm';
import type { OverlayId, MenuState } from '../../types/models';
import type { AppStore } from '../types';
import type { SliceCreator } from './shared';
import { getClipBuf, setClipBuf, sftpSession, getToastTimer, setToastTimer, pendingKinds } from './guards';

export interface OverlaySlice {
  // 覆盖层
  overlay: Overlay_ | null; menu: MenuState | null; toast: string | null;
  hostkey: HostKeyState_ | null;
  confirm: ConfirmState_ | null; input: InputState_ | null;
  palQuery: string; palSel: number;
  encOpen: boolean;

  showToast(msg: string): void;
  openSheet(id: Extract<OverlayId,'hostform'|'hostdetail'|'import'|'backup'|'settings'|'coverage'|'conninfo'|'small'>, arg?: string): void;
  openPalette(): void; closeOverlay(): void;
  askConfirm(fn: string, arg: string, title: string, text: string): void;
  askInput(side: 'local'|'remote', title: string, text: string): void;
  askRename(side: 'local'|'remote', name: string): void;
  /** 主机页右键「新增分组」：打开命名弹窗 */
  newGroup(): void;
  /** 提交新增分组：落盘 folders 后刷新配置 */
  groupSave(): Promise<void>;
  askRenameGroup(name: string): void;
  renameGroupSave(): Promise<void>;
  askDeleteGroup(name: string): void;
  deleteGroup(name: string): Promise<void>;
  confirmYes(): void; inputSave(): void; renameSave(): Promise<void>;
  setPalQuery(q: string): void; palMove(d: number): void; palRun(): void;
  openContextMenu(m: MenuState): void;
  /** 菜单动作：复制路径 / 复制终端选中 / 全选 / 粘贴 / 清屏 */
  copyPathOf(side: 'local'|'remote', name: string): void;
  copyTermSelection(sessionId: string): void;
  selectAllTerminal(sessionId: string): void;
  pasteIntoTerminal(sessionId: string): Promise<void>;
  clearTerm(sessionId: string): void;
  setEncOpen(b: boolean): void;
}

type Overlay_ = AppStore['overlay'];
type HostKeyState_ = AppStore['hostkey'];
type ConfirmState_ = AppStore['confirm'];
type InputState_ = AppStore['input'];

export const createOverlaySlice: SliceCreator<OverlaySlice> = (set, get) => ({
  overlay: null, menu: null, toast: null,
  hostkey: null, confirm: null, input: null,
  palQuery: '', palSel: 0,
  encOpen: false,

  showToast(msg: string) {
    const prev = getToastTimer();
    if (prev) clearTimeout(prev);
    set({ toast: msg });
    setToastTimer(setTimeout(() => set({ toast: null }), 2100));
  },

  /* ---------- 覆盖层 ---------- */
  openSheet(id, arg) {
    // 切到别的 overlay 时，连接信息面板要停采样（由 ConnInfoSheet 卸载处理）；
    // 编码面板的全局键盘接管随之让位（否则面板开着时 Enter 会被它抢走）。
    set({ overlay: { kind: 'sheet', id, arg }, encOpen: false });
  },
  openPalette() { set({ overlay: { kind: 'palette' }, palQuery: '', palSel: 0, encOpen: false }); },
  closeOverlay() {
    // 关闭表单 Sheet 即丢弃未保存的密码明文；别的 overlay 关闭不动表单
    const o = get().overlay;
    if (o && o.kind === 'sheet' && o.id === 'hostform') set({ form: null });
    // 指纹挑战还挂着时随遮罩 / Esc 关闭：与「拒绝」按钮同语义，通知后端取消 oneshot，
    // 不让握手在后端挂满 120s；显式接受 / 拒绝按钮直接 set 清空、不经过这里，不会重复 reject
    const hk = get().hostkey;
    if (hk) {
      void api.rejectHostKey(hk.sessionId).catch(() => { /* 后端无挂起校验时静默 */ });
      // 与 hostkeyReject 同语义：本次挑战作废，kind 暂存要清掉，未被引用的
      // 快速连接临时主机要回收（否则每次「关闭指纹框」都留下垃圾 Host）。
      pendingKinds.delete(hk.hostId);
      if (!get().sessions.some((x) => x.hostId === hk.hostId)) {
        set({ transientHosts: get().transientHosts.filter((h) => h.id !== hk.hostId) });
      }
      // 还有排队挑战时保留 modal，切到下一项；否则才真正关闭。
      const q = get().hostkeyQueue;
      if (q.length) {
        set({
          hostkey: q[0], hostkeyQueue: q.slice(1), menu: null, pw: null,
          overlay: { kind: 'modal', id: 'hostkey' },
        });
        return;
      }
    }
    set({ overlay: null, menu: null, pw: null, ...(hk ? { hostkey: null } : {}) });
  },
  askConfirm(fn, arg, title, text) {
    set({ confirm: { fn, arg, title, text }, overlay: { kind: 'modal', id: 'confirm' } });
  },
  askInput(side, title, text) {
    set({ input: { side, title, text, value: '', mode: 'mkdir' }, overlay: { kind: 'modal', id: 'input' } });
  },
  askRename(side, name) {
    set({
      input: {
        side, title: T('rn.title'), text: T('rn.body', { 0: name }), value: name, mode: 'rename', from: name,
      },
      overlay: { kind: 'modal', id: 'input' },
    });
  },
  /** 主机页右键「新增分组」：打开命名弹窗（复用通用 InputDialog 的 newgroup 模式）。 */
  newGroup() {
    set({
      input: {
        side: 'local', title: T('grp.title'), text: T('grp.body'), value: '', mode: 'newgroup',
      },
      overlay: { kind: 'modal', id: 'input' },
    });
  },
  /** 提交新增分组：落盘 folders 后重新拉配置（后端会在 folders 为空时用现有 host.group 播种）。 */
  async groupSave() {
    const inp = get().input;
    if (!inp || inp.mode !== 'newgroup') return;
    const name = inp.value.trim();
    if (!name) { get().showToast(T('toast.needName')); return; }
    if (get().folders.some((f) => f.name === name)) {
      get().showToast(T('toast.groupExists', { 0: name }));
      return;
    }
    try {
      await api.upsertFolder(name);
      set({ overlay: null, input: null });
      await get().reload();
      get().showToast(T('toast.groupAdded', { 0: name }));
    } catch (e) {
      get().showToast(T('err.saveFailed', { 0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail) }));
    }
  },
  /** 分组右键「重命名」：输入弹窗预填原组名（提交走 renameGroupSave）。 */
  askRenameGroup(name) {
    set({
      input: {
        side: 'local', title: T('grp.renameTitle'), text: T('grp.renameBody', { 0: name }),
        value: name, mode: 'grouprename', from: name,
      },
      overlay: { kind: 'modal', id: 'input' },
    });
  },
  /** 提交分组重命名：folders 条目与组内主机的 group 由后端一并更新。 */
  async renameGroupSave() {
    const inp = get().input;
    if (!inp || inp.mode !== 'grouprename' || !inp.from) return;
    const newName = inp.value.trim();
    const oldName = inp.from;
    if (!newName) { get().showToast(T('toast.needName')); return; }
    set({ overlay: null, input: null });
    if (newName === oldName) return;
    try {
      await api.renameFolder(oldName, newName);
      // 折叠态是前端瞬时状态：key 跟随改名迁移，避免新组名默认展开
      const closed = get().closedGroups;
      const nextClosed: Record<string, boolean> = {};
      for (const [k, v] of Object.entries(closed)) nextClosed[k === oldName ? newName : k] = v;
      set({ closedGroups: nextClosed });
      await get().reload();
      get().showToast(T('toast.groupRenamed', { 0: newName }));
    } catch (e) {
      get().showToast(T('err.saveFailed', { 0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail) }));
    }
  },
  /** 分组右键「删除」：确认弹窗（主机不删除，移入剩余第一个分组）。 */
  askDeleteGroup(name) {
    get().askConfirm('delgroup', name, T('del.group'), T('toast.delGroupWarn'));
  },
  /** 提交删除分组：后端返回被移动的主机数，toast 如实告知去向。 */
  async deleteGroup(name) {
    try {
      const moved = await api.deleteFolder(name);
      const closed = get().closedGroups;
      if (closed[name] !== undefined) {
        const nextClosed = { ...closed };
        delete nextClosed[name];
        set({ closedGroups: nextClosed });
      }
      await get().reload();
      get().showToast(T('toast.groupDeleted', { 0: moved }));
    } catch (e) {
      get().showToast(T('err.deleteFailed', { 0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail) }));
    }
  },
  confirmYes() {
    const c = get().confirm;
    set({ overlay: null, confirm: null });
    if (!c) return;
    if (c.fn === 'closetab') void get().closeSession(c.arg);
    else if (c.fn === 'delhost') void get().deleteHostById(c.arg);
    else if (c.fn === 'delfile') {
      // arg 形如 `local|name`：删除只作用于右键的那一侧
      const [side, ...rest] = c.arg.split('|');
      const name = rest.join('|');
      void get().deleteSelectedFile(name ? { side: side as 'local'|'remote', name } : undefined);
    }
    else if (c.fn === 'importbackup') get().applyBackup(c.arg);
    else if (c.fn === 'importbackupfile') get().importBackupFile(c.arg);
    else if (c.fn === 'importinbox') void get().importInbox(c.arg);
    else if (c.fn === 'peerremove') void get().removePeer(c.arg);
    else if (c.fn === 'snipdel') void get().deleteSnippetById(c.arg);
    else if (c.fn === 'ruledel') void get().deleteRuleById(c.arg);
    else if (c.fn === 'auditclear') {
      void api.peerAuditClear().then(() => get().showToast(T('toast.deleted', { 0: T('audit.title') }))).catch(() => { /* ignore */ });
    }
    else if (c.fn === 'delgroup') void get().deleteGroup(c.arg);
    // 清空会话日志（明文终端内容的一键销毁）
    else if (c.fn === 'clearlogs') void get().clearSessionLogs();
  },
  inputSave() { void get().mkdirSave(); },
  async renameSave() {
    const inp = get().input;
    if (!inp || inp.mode !== 'rename' || !inp.from) return;
    const newName = inp.value.trim();
    if (!newName) { get().showToast(T('toast.needName')); return; }
    const side = inp.side;
    const from = inp.from;
    if (newName === from) { set({ overlay: null, input: null }); return; }
    set({ overlay: null, input: null });
    try {
      if (side === 'local') {
        await api.localRename(formatSidePath('local', get().localPath, from), newName);
        set({ selLocal: newName });
      } else {
        const s = sftpSession(get());
        if (!s) throw { code: 'E_NOT_FOUND', detail: T('err.noSession') };
        await api.sftpRename(s.id, formatSidePath('remote', get().remotePath, from), newName);
        set({ selRemote: newName });
      }
      get().showToast(T('toast.renamed', { 0: newName }));
      get().refreshSftp(true);
    } catch (e) {
      get().showToast(T('err.opFailed', { 0: T('rn.title'), 1: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail) }));
    }
  },
  setPalQuery(q) { set({ palQuery: q, palSel: 0 }); },
  palMove(d) { set({ palSel: Math.max(0, get().palSel + d) }); },
  palRun() { void 0; }, // 由 CommandPalette 组件对选中项调用 dispatch('palrun', 'act|arg')
  openContextMenu(m) { set({ overlay: { kind: 'menu', x: m.x, y: m.y, items: m.items } }); },

  /* ---------- 右键菜单动作 ---------- */
  copyPathOf(side, name) {
    const s = get();
    const path = formatSidePath(side, side === 'local' ? s.localPath : s.remotePath, name);
    setClipBuf(path);
    void navigator.clipboard?.writeText(path).catch(() => { /* 权限不可用时仍有应用内缓冲 */ });
    get().showToast(T('toast.copiedPath', { 0: path }));
  },
  copyTermSelection(sessionId) {
    const txt = getTermSelection(sessionId);
    if (!txt.trim()) { get().showToast(T('toast.copyEmpty')); return; }
    setClipBuf(txt);
    void navigator.clipboard?.writeText(txt).catch(() => { /* ignore */ });
    get().showToast(T('toast.copied', { 0: txt.length > 40 ? txt.slice(0, 40) + '…' : txt }));
  },
  selectAllTerminal(sessionId) { selectAllTerm(sessionId); },
  async pasteIntoTerminal(sessionId) {
    // 优先应用内缓冲，避免浏览器剪贴板权限导致异步挂起；只写入输入行，不自动回车
    let text = getClipBuf();
    if (!text) {
      try {
        text = (await navigator.clipboard?.readText()) ?? '';
      } catch {
        get().showToast(T('toast.clipboardDenied'));
        return;
      }
    }
    if (!text) { get().showToast(T('toast.clipboardDenied')); return; }
    pasteToTerminal(sessionId, text);
    focusTerm(sessionId);
  },
  clearTerm(sessionId) { clearTerminal(sessionId); focusTerm(sessionId); },
  setEncOpen(b) { set({ encOpen: b }); },
});
