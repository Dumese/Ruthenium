// 右键菜单的区域解析：整个应用只挂一个 contextmenu 监听，
// 用 data-ctx 标记区分六类区域，再调用 lib/menus.ts 的对应条目表。
//
// 关键约束：**除真输入框外，任何位置都必须 preventDefault**，
// 否则 WebView2 会弹出自带页面菜单（返回 / 刷新 / 另存为 / 打印）——用户看到的就是
// 「WebView2 的右键菜单漏出来了」。只在六个 data-ctx 区域内拦截不够，侧栏头部
// （搜索框旁、分段控件）、标题栏、标签栏空白、状态栏、弹层空白处都会漏。
import { useEffect } from 'react';
import { useAppStore } from '../store/useAppStore';
import { fileMenu, groupMenu, hostMenu, paneMenu, sideMenu, tabMenu, termMenu } from '../lib/menus';
import { hasTermSelection } from '../components/terminal/useXterm';

/** 真输入框：保留系统右键菜单（粘贴 / 全选）。 */
function isRealInput(t: HTMLElement): boolean {
  const el = t.closest('input,textarea,select,[contenteditable="true"]');
  if (!el) return false;
  // xterm 的隐藏输入框不是「用户可编辑的输入框」：对它的右键应按终端处理，
  // 否则终端里右键会弹出系统菜单而不是应用自己的 8 项终端菜单。
  if (el.classList.contains('xterm-helper-textarea')) return false;
  return true;
}

export function useContextMenu(): void {
  useEffect(() => {
    const onCtx = (ev: MouseEvent) => {
      const t = ev.target as HTMLElement | null;
      if (!t) return;
      // 菜单自身不弹菜单
      if (t.closest('.menu')) { ev.preventDefault(); return; }
      // 输入框保留系统菜单：用户需要系统级粘贴与全选
      if (isRealInput(t)) return;
      const s = useAppStore.getState();
      const hostEl = t.closest('[data-ctx="host"]') as HTMLElement | null;
      const tabEl = t.closest('[data-ctx="tab"]') as HTMLElement | null;
      const frowEl = t.closest('[data-ctx="frow"]') as HTMLElement | null;
      const paneEl = t.closest('[data-ctx="pane"]') as HTMLElement | null;
      const termEl = t.closest('[data-ctx="term"]') as HTMLElement | null;
      const sideEl = t.closest('[data-ctx="side"]') as HTMLElement | null;
      // 分组行是 side 的后代：必须在 side 之前判定（否则弹的是侧栏空白菜单）
      const groupEl = t.closest('[data-ctx="group"]') as HTMLElement | null;
      const at = { x: ev.clientX, y: ev.clientY };

      if (groupEl) {
        ev.preventDefault();
        const name = groupEl.dataset.arg ?? '';
        if (!name) return;
        s.openContextMenu({ ...at, items: groupMenu(name) });
        return;
      }

      if (hostEl) {
        ev.preventDefault();
        const id = hostEl.dataset.arg ?? '';
        const host = s.hosts.find((h) => h.id === id);
        if (!host) return;
        s.selectHost(host.id);               // 右键先选中目标
        s.openContextMenu({ ...at, items: hostMenu(host) });
        return;
      }
      if (tabEl) {
        ev.preventDefault();
        const id = tabEl.dataset.arg ?? '';
        const sess = s.sessions.find((x) => x.id === id);
        // 快速连接的临时会话多一项「保存为主机」
        const transient = !!sess && s.transientHosts.some((h) => h.id === sess.hostId);
        s.openContextMenu({ ...at, items: tabMenu(sess, s.sessions, transient) });
        return;
      }
      if (frowEl) {
        ev.preventDefault();
        const raw = frowEl.dataset.arg ?? '';
        const i = raw.indexOf('|');
        const side = raw.slice(0, i) as 'local' | 'remote';
        const name = raw.slice(i + 1);
        s.selectFile(side, name);            // 文件行先进入选中态
        s.openContextMenu({ ...at, items: fileMenu(side, name, frowEl.classList.contains('dir')) });
        return;
      }
      if (paneEl) {
        ev.preventDefault();
        const side = (paneEl.dataset.side === 'local' ? 'local' : 'remote') as 'local' | 'remote';
        s.openContextMenu({ ...at, items: paneMenu(side, s.selLocal, s.selRemote) });
        return;
      }
      if (termEl) {
        ev.preventDefault();
        const active = s.sessions.find((x) => x.id === s.active);
        const sid = active && (active.kind === 'term' || active.kind === 'local' || active.kind === 'peer')
          ? active.id : null;
        s.openContextMenu({ ...at, items: termMenu(sid, sid ? hasTermSelection(sid) : false) });
        return;
      }
      if (sideEl) {
        ev.preventDefault();
        s.openContextMenu({ ...at, items: sideMenu({ hostsTab: s.sideTab === 'hosts' }) });
        return;
      }
      // 兜底：其它任何区域（标题栏 / 标签栏空白 / 状态栏 / 弹层空白…）都不允许
      // 冒出 WebView2 自带菜单；这里只拦截、不弹自绘菜单（这些位置没有可执行的动作）。
      ev.preventDefault();
    };
    // capture 阶段：即使某个子元素（如 xterm）在冒泡阶段 stopPropagation，
    // 也一定能拦住原生菜单。
    document.addEventListener('contextmenu', onCtx, true);
    return () => document.removeEventListener('contextmenu', onCtx, true);
  }, []);
}
