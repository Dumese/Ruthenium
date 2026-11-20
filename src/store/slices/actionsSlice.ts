// 动作分发 slice：runNamed（命令面板）/ dispatch（data-act 事件委托）/ goto（覆盖表）。
// 三张表都是「字符串 → 已有 action」的纯转发，不含业务逻辑；拆出来是为了让
// sessionSlice 专注连接/会话语义，这里专注 UI 动作路由。
import { T } from '../../i18n';
import { hostMenu } from '../../lib/menus';
import { useMock } from '../../ipc/env';
import { isDevOrE2E } from '../../lib/buildMode';
import { mockDisconnectSession } from '../../ipc/mockBridge';
import * as api from '../../ipc/commands';
import type { SideTab } from '../../types/models';
import type { SliceCreator } from './shared';

export interface ActionsSlice {
  dispatch(act: string, arg?: string): void;
  runNamed(n: string): void;
  goto(k: string): void;
}

export const createActionsSlice: SliceCreator<ActionsSlice> = (set, get) => ({
  /* ---------- 命令面板动作表 ---------- */
  runNamed(n) {
    const s = get();
    switch (n) {
      case 'hostform': s.openHostForm(); break;
      case 'import': s.openImport(); break;
      case 'backup': s.openSheet('backup'); break;
      case 'settings': s.openSheet('settings'); break;
      // 覆盖表是 dev/QA 演示入口（会触发真实删除/关闭动作），生产构建不可达。
      case 'coverage': if (isDevOrE2E) s.openSheet('coverage'); break;
      case 'conninfo': s.openSheet('conninfo'); break;
      case 'encpick': s.setEncOpen(!s.encOpen); break;
      case 'tabsftp': {
        const a = s.sessions.find((x) => x.id === s.active);
        // 不拿 hosts[0] 兜底：没有活动会话就提示
        void s.openSftpFor(a?.hostId ?? '');
        break;
      }
      case 'theme': s.toggleTheme(); break;
      case 'sidebar': s.toggleSidebar(); break;
      case 'focus': s.toggleFocus(); break;
      case 'setnavx': s.setNav('x'); break;
      case 'setnavclassic': s.setNav('classic'); break;
      case 'closeall': void s.closeAll(); break;
      case 'snipnew': s.openSnipForm(); break;
      case 'newgroup': s.newGroup(); break;
      case 'groupsave': void s.groupSave(); break;
      case 'rulenew': s.openRuleForm(); break;
      case 'palette': s.openPalette(); break;
      default: break;
    }
  },

  /* ---------- data-act 事件委托等价入口 ---------- */
  dispatch(act, arg = '') {
    const s = get();
    switch (act) {
      /* chrome */
      case 'palette': s.openPalette(); break;
      case 'closeov': s.closeOverlay(); break;
      case 'hostform': s.openHostForm(); break;
      case 'sidebar': s.toggleSidebar(); break;
      case 'theme': s.toggleTheme(); break;
      case 'focus': s.toggleFocus(); break;
      case 'settings': s.openSheet('settings'); break;
      // 覆盖表是 dev/QA 演示入口（会触发真实删除/关闭动作），生产构建不可达。
      case 'coverage': if (isDevOrE2E) s.openSheet('coverage'); break;
      case 'conninfo': s.openSheet('conninfo'); break;
      case 'encpick': s.setEncOpen(!s.encOpen); break;
      case 'encset': void s.applyEncoding(arg); break;
      case 'metricstoggle': s.toggleMetricsLive(); break;
      case 'metricsrefresh': {
        const a = s.active;
        if (a) void api.sampleMetrics(a).then((m) => get().onMetricsSample(m)).catch(() => s.showToast(T('toast.metricsNeedConn')));
        break;
      }
      case 'import': s.openImport(); break;
      case 'backup': s.openSheet('backup'); break;
      case 'goto': s.goto(arg); break;
      case 'navclassic': case 'setnavclassic': s.setNav('classic'); break;
      case 'setnavx': s.setNav('x'); break;
      case 'sidetab': s.setSideTab(arg as SideTab); break;
      case 'group': s.toggleGroup(arg); break;
      case 'datatab': s.setActive(arg); break;
      case 'tab': s.setActive(arg); break;
      case 'tabclose': {
        const sess = s.sessions.find((x) => x.id === arg);
        if (s.confirmClose && sess) s.askConfirm('closetab', arg, T('del.tab'), T('toast.closeTabQ', { 0: sess.title }));
        else void s.closeSession(arg);
        break;
      }
      case 'newtab': void s.requestConnect('', 'local'); break;
      case 'tabsftp': s.runNamed('tabsftp'); break;
      case 'disconnect': {
        // 标签栏「模拟断线」：**仅 mock 演示**，用来展示断线横幅与重连。
        // 生产构建已剔除 mock 且 TabBar 不渲染该按钮；真实后端环境下绝不伪造 conn://state。
        if (!useMock) break;
        void s.ensureTerm().then((t) => {
          if (!t) return;
          mockDisconnectSession(t.id);
          s.showToast(T('toast.discHint'));
        });
        break;
      }
      case 'reconnect': void s.reconnect(arg || s.active || ''); break;
      /* 自动重连倒计时上的「立即重连 / 取消」与临时主机「保存为主机」 */
      case 'retrynow': s.retryNow(arg || s.active || ''); break;
      case 'retrycancel': s.cancelAutoReconnect(arg || s.active || ''); break;
      case 'savequickhost': s.saveQuickHost(arg || s.active || ''); break;
      case 'openlogdir': void s.openLogFolder(); break;
      case 'clearlogs': void s.clearSessionLogs(); break;
      case 'termfind': s.openTermFind(); break;
      case 'closetabconn': void s.closeSession(arg); break;
      case 'closedisc': { const a = arg || s.active; if (a) void s.closeSession(a); break; }
      case 'closeall': void s.closeAll(); break;
      case 'closeothers': void s.closeOthers(arg); break;
      case 'closeright': void s.closeRight(arg); break;
      /* hosts */
      case 'host': s.selectHost(arg); break;
      /* 所有连接入口统一走 requestConnect 闸门 */
      case 'connect': void s.requestConnect(arg, 'term'); break;
      case 'opensftp': void s.requestConnect(arg, 'sftp'); break;
      case 'hostdetail': s.openSheet('hostdetail', arg); break;
      case 'hostedit': { const h = s.hosts.find((x) => x.id === arg); if (h) s.openHostForm(h); break; }
      case 'hostdel': s.askConfirm('delhost', arg, T('del.host'), T('toast.delHostWarn')); break;
      case 'copyssh': s.copySsh(arg); break;
      case 'hostkeyaccept': void s.hostkeyAccept(); break;
      case 'hostkeyreject': void s.hostkeyReject(); break;
      case 'confirmyes': s.confirmYes(); break;
      case 'mkdirsave': s.inputSave(); break;
      case 'renamesave': void s.renameSave(); break;
      case 'frename': { const p = arg.split('|'); s.askRename(p[0] as 'local'|'remote', p.slice(1).join('|')); break; }
      case 'copypath': { const p = arg.split('|'); s.copyPathOf(p[0] as 'local'|'remote', p.slice(1).join('|')); break; }
      /* 终端右键菜单动作（粘贴只写入输入行，不自动回车） */
      case 'copyterm': s.copyTermSelection(arg || s.active || ''); break;
      case 'termselall': s.selectAllTerminal(arg || s.active || ''); break;
      case 'termpaste': void s.pasteIntoTerminal(arg || s.active || ''); break;
      case 'termclear': s.clearTerm(arg || s.active || ''); break;
      /* 表单 */
      case 'formsave': void s.saveForm(); break;
      case 'savepw': void s.saveFormPassword(); break;
      /* 导入 */
      case 'impparse': void s.parseImportText(); break;
      case 'imptoggle': s.toggleImportRow(Number(arg)); break;
      case 'impback': s.importBack(); break;
      case 'impdo': void s.importSelectedRows(); break;
      case 'export': s.exportBackupJson(); break;
      case 'importjson': document.getElementById('backupFileInput')?.click(); break;
      /* 片段与转发 */
      case 'snipnew': s.openSnipForm(); break;
      case 'newgroup': s.newGroup(); break;
      case 'groupsave': void s.groupSave(); break;
      case 'grouprename': s.askRenameGroup(arg); break;
      case 'groupdel': s.askDeleteGroup(arg); break;
      case 'grouprenamesave': void s.renameGroupSave(); break;
      case 'snipedit': { const n = s.snippets.find((x) => x.id === arg); if (n) s.openSnipForm(n); break; }
      case 'snipdel': void s.deleteSnippetById(arg); break;
      case 'snipinsert': void s.insertSnippet(arg); break;
      case 'rulenew': s.openRuleForm(undefined, typeof arg === 'string' && arg ? arg : undefined); break;
      case 'ruleedit': { const r = s.tunnels.find((x) => x.id === arg); if (r) s.openRuleForm(r); break; }
      case 'ruledel': void s.deleteRuleById(arg); break;
      case 'ruletoggle': void s.toggleRule(arg); break;
      /* SFTP */
      // 文件名允许含 `|`（POSIX 远端）——`side|name` 消费端一律取首个 `|` 后的**整段**
      case 'fsel': { const i = arg.indexOf('|'); s.selectFile(arg.slice(0, i) as 'local'|'remote', arg.slice(i + 1)); break; }
      case 'fopen': {
        // 双击文件：选中并提示；目录由 FilePane 直接调用 openDir
        const i = arg.indexOf('|');
        const name = arg.slice(i + 1);
        s.selectFile(arg.slice(0, i) as 'local'|'remote', name);
        s.showToast(T('toast.isFile', { 0: name }));
        break;
      }
      case 'fopendir': {
        const i = arg.indexOf('|');
        s.openDir(arg.slice(0, i) as 'local'|'remote', arg.slice(i + 1));
        break;
      }
      case 'fpath': { const i = arg.indexOf('|'); s.setSftpPath(arg.slice(0, i) as 'local'|'remote', arg.slice(i + 1)); break; }
      case 'fup': s.upDir(arg as 'local'|'remote'); break;
      case 'frefresh': s.refreshSftp(); break;
      case 'fnew': s.askInput(arg as 'local'|'remote', T('mk.title'), T('mk.body', { 0: arg === 'local' ? T('mk.local') : T('mk.remote') })); break;
      case 'fdelete': {
        // arg 形如 `side|name`（右键菜单传入）：确认弹窗里再引用，确保删的是右键那侧
        const p = arg.includes('|') ? arg.split('|') : null;
        const side = (p?.[0] as 'local'|'remote' | undefined) ?? (s.selRemote ? 'remote' : s.selLocal ? 'local' : 'local');
        const name = p ? p.slice(1).join('|') : (side === 'local' ? s.selLocal : s.selRemote) ?? '';
        s.askConfirm('delfile', arg, T('del.file'), T('toast.delFileWarn', { 0: name }));
        break;
      }
      case 'upload': {
        const p = arg.includes('|') ? arg.split('|') : null;
        void s.doTransfer('up', p ? { name: p.slice(1).join('|') } : undefined);
        break;
      }
      case 'download': {
        const p = arg.includes('|') ? arg.split('|') : null;
        void s.doTransfer('down', p ? { name: p.slice(1).join('|') } : undefined);
        break;
      }
      case 'qclear': s.clearQueue(); break;
      /* 命令面板 */
      case 'palrun': {
        const [a, b] = arg.split('|');
        s.closeOverlay();
        if (a === 'palrun') s.runNamed(b);
        else s.dispatch(a, b);
        break;
      }
      default: break;
    }
  },

  /* ---------- 覆盖表 goto（28 项） ---------- */
  goto(k) {
    const s = get();
    /** 没有目标主机时只提示并回到主机库，绝不自动挑一台主机建会话。 */
    const backToHosts = (msg: string) => {
      s.setNav('classic');
      set({ collapsed: false, sideTab: 'hosts' });
      s.closeOverlay();
      s.showToast(msg);
    };
    switch (k) {
      case 'hosts':
        s.setNav('classic'); set({ sideTab: 'hosts' }); s.closeOverlay(); s.showToast(T('toast.hostsSide')); break;
      case 'groupsearch':
        s.setNav('classic'); set({ sideTab: 'hosts' }); s.closeOverlay();
        setTimeout(() => { const e = document.getElementById('sideSearch') as HTMLInputElement | null; if (e) e.focus(); }, 30);
        s.showToast(T('toast.filterHint')); break;
      case 'hostform': s.openHostForm(); break;
      case 'hostdetail': set({ nav: 'classic' }); s.openSheet('hostdetail', s.hosts[0]?.id); break;
      case 'hostdel': {
        const last = s.hosts[s.hosts.length - 1];
        if (last) s.askConfirm('delhost', last.id, T('del.host'), T('toast.delHostWarn'));
        break;
      }
      case 'import': s.openImport(); break;
      case 'hostkey': {
        // 覆盖表是**演示**入口，会在前端凭空构造 sessionId 与硬编码指纹。
        // 生产包里绝不允许出现"不校验真实指纹的指纹弹窗"，只在 dev / e2e 构建保留。
        // 这里刻意内联 mode 判断（而不是走 isDevOrE2E 常量）：生产构建要求 esbuild
        // 静态折叠该分支并丢弃下面的指纹字面量。
        if (!(import.meta.env.DEV || import.meta.env.MODE === 'e2e')) break;
        const h = s.hosts.find((x) => !x.known) ?? s.hosts[0];
        if (h) s.onHostkeyChallenge({ sessionId: 'pending-' + h.id, hostId: h.id, host: h.name, port: h.port || 22, algo: 'ED25519', fingerprint: 'SHA256:mZ4K9xVQ2rT8yH1pLcN6aF0sD3gB7wE5jR2uY9iO4qA', changed: false });
        break;
      }
      case 'connect': case 'tabs':
        // 不拿 hosts[0] 兜底建会话；没有会话时把用户送回主机库自行选择。
        s.setNav('classic'); set({ collapsed: false, sideTab: 'hosts' }); s.closeOverlay();
        if (!s.sessions.length) s.showToast(T('empty.body'));
        break;
      case 'disconnect':
        s.dispatch('disconnect'); s.closeOverlay(); break;
      case 'empty':
        void s.closeAll(); s.closeOverlay(); s.showToast(T('toast.closedAll')); break;
      case 'terminal': {
        // 「终端命令输入」演示：没有活动会话时建本地终端（不拿任意主机兜底）
        void s.ensureTerm().then((t) => {
          if (t) { s.closeOverlay(); s.showToast(T('toast.helpHint')); return; }
          void s.requestConnect('', 'local').then(() => { s.closeOverlay(); s.showToast(T('toast.helpHint')); });
        });
        break;
      }
      case 'snippets':
        s.setNav('classic'); set({ sideTab: 'snips' }); s.closeOverlay(); break;
      case 'snipinsert': {
        const first = s.snippets[0];
        if (first) void s.insertSnippet(first.id);
        break;
      }
      case 'sftp': {
        // 「SFTP 双栏」演示入口：只复用当前 / 已有会话的主机；没有目标主机时
        // 回主机库让用户显式选择（禁止自动挑一台主机建会话）。
        s.closeOverlay();
        const a = s.sessions.find((x) => x.id === s.active && x.hostId) ?? s.sessions.find((x) => x.hostId);
        if (a) { void s.openSftpFor(a.hostId); break; }
        backToHosts(T('sftpPage.hint'));
        break;
      }
      case 'transfer': {
        void (async () => {
          // 演示入口同样优先活动 SFTP 会话，避免在多主机并存时切换到别的主机；
          // 无 SFTP 会话时复用已有终端会话的 host 建一个（禁止自动挑主机）。
          let sf = s.sessions.find((x) => x.id === s.active && x.kind === 'sftp')
            ?? s.sessions.find((x) => x.kind === 'sftp');
          if (!sf) {
            const live = s.sessions.find((x) => x.kind === 'term' && x.hostId);
            if (live) { await s.requestConnect(live.hostId, 'sftp'); sf = get().sessions.find((x) => x.kind === 'sftp'); }
          }
          if (!sf) { backToHosts(T('sftpPage.hint')); return; }
          set({ active: sf.id, localPath: '~\\Downloads', selLocal: 'schema.sql' });
          s.closeOverlay();
          void s.doTransfer('up');
        })();
        break;
      }
      case 'rules':
        s.setNav('classic'); set({ sideTab: 'rules' }); s.closeOverlay(); break;
      case 'ruletoggle': {
        const r = get().tunnels[0];
        if (r) void s.toggleRule(r.id);
        s.closeOverlay();
        break;
      }
      case 'palette': s.openPalette(); break;
      case 'settings': s.openSheet('settings'); break;
      case 'theme': s.toggleTheme(); s.closeOverlay(); break;
      case 'backup': s.openSheet('backup'); break;
      case 'focus': set({ focus: true }); s.closeOverlay(); break;
      case 'sidebar': s.setNav('classic'); set({ collapsed: true }); s.closeOverlay(); break;
      case 'xnav': s.setNav('x'); break;
      case 'status': set({ showStatus: true }); s.closeOverlay(); s.showToast(T('toast.statusOn')); break;
      /* 连接信息 / 语言 / 编码 */
      case 'conninfo': {
        s.closeOverlay();
        if (!s.active) { s.showToast(T('ci.empty')); return; }
        s.openSheet('conninfo');
        break;
      }
      case 'lang':
        s.openSheet('settings', 'lang');
        break;
      case 'encoding':
        s.closeOverlay();
        if (!s.active) { s.showToast(T('enc.noSession')); return; }
        set({ encOpen: true });
        break;
      /* 认证方式 / 右键菜单 */
      case 'auth': {
        // 打开主机表单并停在认证区：优先选一台密码认证的主机（能看到状态行）
        s.setNav('classic');
        set({ collapsed: false, sideTab: 'hosts' });
        const target = s.hosts.find((x) => x.auth === 'password') ?? s.hosts.find((x) => x.auth !== 'key');
        s.openHostForm(target);
        setTimeout(() => {
          const el = document.getElementById('authField');
          el?.scrollIntoView({ block: 'center' });
          (el?.querySelector('.pill') as HTMLElement | null)?.focus();
        }, 80);
        break;
      }
      case 'ctxmenu': {
        // 打开一个真实的示例右键菜单（主机行）
        s.setNav('classic');
        set({ collapsed: false, sideTab: 'hosts' });
        s.closeOverlay();
        const host = s.hosts[0];
        if (!host) { s.openHostForm(); break; }
        s.selectHost(host.id);
        setTimeout(() => {
          const el = document.querySelector('.hostitem') as HTMLElement | null;
          const r = el?.getBoundingClientRect();
          get().openContextMenu({
            x: r ? Math.round(r.right - 24) : 320,
            y: r ? Math.round(r.top + r.height) : 220,
            items: hostMenu(host),
          });
        }, 40);
        break;
      }
      default: s.closeOverlay(); break;
    }
  },
});
