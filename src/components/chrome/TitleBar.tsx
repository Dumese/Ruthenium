// 标题栏（Windows 版）：
// 左侧 Ruthenium 词标 + .tbcaret 4×10 光标块；右侧 ─ □ × 三段 46×40 窗口按钮。
// 三键区域不挂 data-tauri-drag-region；其余区域可拖拽、双击切换最大化。
// 最大化按钮上另有一个原生覆盖层（Windows 11 贴靠布局，见 useSnapOverlay）。
import { useRef, useSyncExternalStore } from 'react';
import { Icon } from '../ui/Icon';
import { IconButton } from '../ui/IconButton';
import { useAppStore } from '../../store/useAppStore';
import { T, useLang } from '../../i18n';
import { isTauri } from '../../ipc/env';
import { useSnapOverlay } from './useSnapOverlay';

function WinCtl() {
  const maxRef = useRef<HTMLButtonElement>(null);
  // 原生覆盖层压在最大化按钮上时，webview 收不到 :hover / :active，
  // 由后端转发的事件驱动这两个类名（见 useSnapOverlay）。
  const snap = useSnapOverlay(maxRef);
  const maxCls = [snap.hover ? 'snap-hover' : '', snap.press ? 'snap-press' : ''].filter(Boolean).join(' ') || undefined;
  const onMin = async () => {
    if (!isTauri) return;
    const { getCurrentWindow } = await import('@tauri-apps/api/window');
    void getCurrentWindow().minimize();
  };
  const onMax = async () => {
    if (!isTauri) return;
    const { getCurrentWindow } = await import('@tauri-apps/api/window');
    void getCurrentWindow().toggleMaximize();
  };
  const onClose = async () => {
    if (!isTauri) return;
    const { getCurrentWindow } = await import('@tauri-apps/api/window');
    void getCurrentWindow().close();
  };
  return (
    <div className="winctl">
      <button title={T('app.winMin')} onClick={() => void onMin()}>{'\u2500'}</button>
      {/* 点击由原生覆盖层处理（WM_NCLBUTTONUP → SC_MAXIMIZE/SC_RESTORE）；
          onClick 只在覆盖层不存在的环境（浏览器 mock、非 Windows）生效。 */}
      <button ref={maxRef} className={maxCls} title={T('app.winMax')} onClick={() => void onMax()}>{'\u25A1'}</button>
      <button className="wclose" title={T('app.winClose')} onClick={() => void onClose()}>{'\u2715'}</button>
    </div>
  );
}

export function TitleBar() {
  useLang(); // 语言切换时重渲染标题栏文案
  const collapsed = useAppStore((s) => s.collapsed);
  const focus = useAppStore((s) => s.focus);
  const theme = useAppStore((s) => s.theme);
  const dispatch = useAppStore((s) => s.dispatch);
  // system 主题跟随 OS 深浅：matchMedia 变化不会触发 React 重渲染，必须显式订阅，
  // 否则 OS 深浅翻转后 sun/moon 图标不更新。桌面端无 SSR，直接组件内订阅即可。
  const systemDark = useSyncExternalStore(
    (onChange) => {
      const mq = window.matchMedia('(prefers-color-scheme: dark)');
      mq.addEventListener('change', onChange);
      return () => mq.removeEventListener('change', onChange);
    },
    () => window.matchMedia('(prefers-color-scheme: dark)').matches,
  );
  const resolvedDark = theme === 'dark' || (theme === 'system' && systemDark);
  return (
    <header
      className="titlebar"
      data-tauri-drag-region=""
      onDoubleClick={(e) => {
        if (!isTauri) return;
        const target = e.target as HTMLElement | null;
        if (target?.closest('button,input,select,.winctl')) return;
        void import('@tauri-apps/api/window').then(({ getCurrentWindow }) => getCurrentWindow().toggleMaximize());
      }}
    >
      <span className="tbbrand">
        <span className="tbtitle">Ruthenium</span>
        <span className="tbcaret" />
      </span>
      <span className="tbsub">· {T('app.tagline')}</span>
      <span className="tbsep" />
      <button className="cmdtrigger" onClick={() => dispatch('palette')}>
        <Icon n="search" s={13} />
        <span>{T('tb.search')}</span>
        <span className="sp" />
        <span className="kbd">Ctrl K</span>
      </button>
      <span className="tbspacer" />
      <IconButton icon="plus" title={T('tb.newHost')} onClick={() => dispatch('hostform')} />
      <IconButton icon="sidebar" title={T('tb.toggleSide')} on={collapsed} onClick={() => dispatch('sidebar')} />
      <IconButton icon={resolvedDark ? 'sun' : 'moon'} title={T('tb.theme')} onClick={() => dispatch('theme')} />
      <IconButton icon="monitor" title={T('tb.focus')} on={focus} onClick={() => dispatch('focus')} />
      <IconButton icon="gear" title={T('tb.settings')} onClick={() => dispatch('settings')} />
      <WinCtl />
    </header>
  );
}
