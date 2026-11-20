// 快捷键（Windows 单平台，仅 Ctrl）。
// MUST：只判断 ev.ctrlKey；在 keydown capture 阶段对全部接管组合键 preventDefault()，
// 避免 WebView2 把 Ctrl+F / Ctrl+L / Ctrl+T 交给内置查找、打印或窗口行为。
// 终端聚焦时同样生效：调用 stopPropagation() 阻止 xterm 再把同一组合键写进 PTY
// （useXterm 另用 attachCustomKeyEventHandler 双保险）。Ctrl+W 永远不拦截，留给 shell 删词。
import { useEffect } from 'react';
import { useAppStore } from '../store/useAppStore';
import { clearTerminal, isTermFocused } from '../components/terminal/useXterm';

// 接管的组合键（在 capture 阶段一律 preventDefault）
function appChord(e: KeyboardEvent): boolean {
  if (!e.ctrlKey || e.altKey || e.metaKey) return false;
  if (e.key === 'Tab') return true;
  const k = e.key.toLowerCase();
  if (k === 'w') return e.shiftKey; // 仅 Ctrl+Shift+W；Ctrl+W 交给 shell
  if (k === 'k' || k === 't' || k === 'n' || k === 'b' || k === ',' || k === 'f' || k === 'l') return true;
  return /^[1-9]$/.test(k);
}

function focusActiveTerm(): void {
  setTimeout(() => {
    const el = document.querySelector('.xterm-helper-textarea') as HTMLTextAreaElement | null;
    el?.focus();
  }, 20);
}

function cycleSession(shift: boolean): void {
  const s = useAppStore.getState();
  const list = s.sessions;
  if (!list.length) return;
  const i = list.findIndex((x) => x.id === s.active);
  let n: number;
  if (i < 0) n = shift ? list.length - 1 : 0;
  else n = (i + (shift ? -1 : 1) + list.length) % list.length;
  s.setActive(list[n].id);
}

export function useHotkeys() {
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const s = useAppStore.getState();

      // Esc：优先关闭终端查找条，再关编码面板 / overlay（输入框内由组件自行处理）
      if (e.key === 'Escape') {
        if (s.findOpen) { e.preventDefault(); s.closeTermFind(); return; }
        if (s.encOpen) { e.preventDefault(); s.setEncOpen(false); return; }
        // 对端确认弹窗渲染在 OverlayHost 之后（视觉上更上层），Esc 必须先处理它们，
        // 否则会关掉底层的设置/表单 Sheet 而把确认框留在屏幕上。
        if (s.screenReq) { e.preventDefault(); e.stopPropagation(); void s.screenConsent(false); return; }
        if (s.controlReq) { e.preventDefault(); e.stopPropagation(); void s.consentControl(false); return; }
        if (s.pair) {
          e.preventDefault();
          e.stopPropagation();
          if (!s.pairBusy) s.pairCancel();
          return;
        }
        if (s.overlay) {
          e.preventDefault();
          e.stopPropagation();
          s.closeOverlay();
          focusActiveTerm();
          return;
        }
        return;
      }

      // 安全确认弹窗打开时，除 Esc 外的全局组合键都不应把焦点移到背后界面。
      if (s.pair || s.controlReq || s.screenReq) return;
      if (!e.ctrlKey || e.altKey || e.metaKey) return; // 仅 Ctrl

      // Ctrl+W 必须完全不拦截：终端里是 shell 的「删除前一个词」
      if (e.key.toLowerCase() === 'w' && !e.shiftKey) return;

      if (!appChord(e)) return;
      // capture 阶段拦截默认行为，并阻止事件继续进入 xterm 的 target 级 keydown
      e.preventDefault();
      e.stopPropagation();

      // Ctrl+Tab / Ctrl+Shift+Tab：循环切换标签（首尾相接）
      if (e.key === 'Tab') {
        cycleSession(e.shiftKey);
        return;
      }

      const k = e.key.toLowerCase();

      // Ctrl+Shift+W：关闭当前标签（confirmClose 时先确认）
      if (k === 'w' && e.shiftKey) {
        const cur = s.sessions.find((x) => x.id === s.active);
        if (cur) s.dispatch('tabclose', cur.id);
        return;
      }

      // Ctrl+T：新建本地终端（不是命令面板）；仍走统一连接闸门
      if (k === 't') {
        s.closeOverlay();
        s.dispatch('newtab');
        return;
      }

      if (k === 'k') { s.openPalette(); return; }
      if (k === 'n') { s.closeOverlay(); s.openHostForm(); return; }
      if (k === ',') { s.openSheet('settings'); return; }
      if (k === 'b') { s.toggleSidebar(); return; }
      if (k === 'f' && e.shiftKey) { s.toggleFocus(); return; }
      if (k === 'f') {
        // Ctrl+F 双上下文分发：
        // - 焦点在终端里（xterm 的 helper textarea）或查找条已打开 → 终端内查找；
        // - 其它位置（侧栏 / 表单 / 空状态）→ 仍是「聚焦侧栏搜索框」。
        // 两路都已在 capture 阶段 preventDefault，WebView2 拿不到 Ctrl+F。
        if (s.findOpen || isTermFocused()) { s.openTermFind(); return; }
        s.setNav('classic');
        useAppStore.setState({ collapsed: false });
        setTimeout(() => (document.getElementById('sideSearch') as HTMLInputElement | null)?.focus(), 30);
        return;
      }
      if (k === 'l') {
        // 只在终端聚焦时接管：焦点在输入框/表单时应交给系统或输入框（否则会误清屏）。
        if (!isTermFocused()) return;
        clearTerminal(s.active ?? '');
        return;
      }
      if (/^[1-9]$/.test(k)) {
        const target = s.sessions[Number(k) - 1];
        if (target) s.setActive(target.id);
      }
    };
    window.addEventListener('keydown', onKey, true); // capture 阶段
    return () => window.removeEventListener('keydown', onKey, true);
  }, []);
}
