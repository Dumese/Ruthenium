// 组装 chrome / stage / overlay；无 router，纯状态驱动
import { useEffect } from 'react';
import './styles/tokens.css';
import './styles/base.css';
import './styles/components.css';
import './styles/terminal.css';
import '@xterm/xterm/css/xterm.css';

import { useAppStore } from './store/useAppStore';
import { useTheme } from './hooks/useTheme';
import { useHotkeys } from './hooks/useHotkeys';
import { useContextMenu } from './hooks/useContextMenu';
import { TitleBar } from './components/chrome/TitleBar';
import { Sidebar } from './components/chrome/Sidebar';
import { Splitter } from './components/chrome/Splitter';
import { TabBar } from './components/chrome/TabBar';
import { StatusBar } from './components/chrome/StatusBar';
import { TerminalView } from './components/terminal/TerminalView';
import { SftpView } from './components/sftp/SftpView';
import { VaultsPage } from './components/data/VaultsPage';
import { SnippetsPage } from './components/data/SnippetsPage';
import { RulesPage } from './components/data/RulesPage';
import { OverlayHost } from './components/overlays/OverlayHost';
import { PeerPairDialog } from './components/peers/PeerPairDialog';
import { PeerControlDialog } from './components/peers/PeerControlDialog';
import { PeerControlBanner } from './components/peers/PeerControlBanner';
import { PeerScreenDialog } from './components/peers/PeerScreenDialog';
import { PeerPanel } from './components/peers/PeerPanel';
import { RemoteView } from './components/screen/RemoteView';
import { EncodingPicker } from './components/terminal/EncodingPicker';
import { ToastHost } from './components/overlays/ToastHost';
import { Button } from './components/ui/Button';
import { Icon } from './components/ui/Icon';
import { viewState } from './store/selectors';
import { listen } from './ipc/events';
import { EV } from './types/events';
import type { DiscoveredDevice, PairEvent, InboxNotice, ControlRequest, ControlSessionEvent, PeerDeniedEvent, ScreenRequest, ScreenSessionEvent } from './ipc/commands';
import type { InputStatsEvent, ClipboardEndedEvent } from './types/events';
import { writeTermData } from './components/terminal/useXterm';
import { T, useLang } from './i18n';
import type { ConnStateEvent, HostkeyChallengeEvent, SftpProgressEvent, SftpChangedEvent, TermDataEvent, TermExitEvent, TunnelStateEvent, AppErrorEvent, MetricsSampleEvent } from './types/events';

function EmptyState() {
  useLang();
  const dispatch = useAppStore((s) => s.dispatch);
  return (
    <div className="empty">
      <div className="eicon"><Icon n="terminal" s={28} /></div>
      <h2>{T('empty.title')}</h2>
      <p>{T('empty.body')}</p>
      <div className="erow">
        <Button icon="search" primary onClick={() => dispatch('palette')}>{T('empty.search')}</Button>
        <Button icon="plus" onClick={() => dispatch('hostform')}>{T('empty.newHost')}</Button>
        <Button icon="download" onClick={() => dispatch('import')}>{T('empty.import')}</Button>
      </div>
      <div className="ehint"><kbd>Ctrl T</kbd> {T('empty.hintNew')}　<kbd>Ctrl K</kbd> {T('empty.hintPalette')}　<kbd>Ctrl B</kbd> {T('empty.hintSide')}</div>
    </div>
  );
}

function SftpPageEmpty() {
  useLang();
  const sessions = useAppStore((s) => s.sessions);
  const dispatch = useAppStore((s) => s.dispatch);
  return (
    <div className="empty">
      <div className="eicon"><Icon n="folder" s={28} /></div>
      <h2>{T('sftpPage.empty')}</h2>
      <p>{T('sftpPage.hint')}</p>
      <div className="erow">
        <Button
            icon="folder"
            primary
            disabled={!sessions[0]?.hostId}
            onClick={() => dispatch('opensftp', sessions[0]?.hostId ?? '')}
          >{T('sftpPage.open')}</Button>
        <Button onClick={() => dispatch('datatab', 'vaults')}>{T('sftpPage.back')}</Button>
      </div>
    </div>
  );
}

function PeersPage() {
  useLang();
  return (
    <div className="datapage">
      <div className="dphead">
        <div>
          <h2>{T('side.tabPeers')}</h2>
        </div>
      </div>
      <div className="peerspage"><PeerPanel /></div>
    </div>
  );
}

function Stage() {
  const active = useAppStore((s) => s.active);
  const sessions = useAppStore((s) => s.sessions);
  const vs = viewState();
  let content: React.ReactNode;
  switch (vs) {
    case 'vaults': content = <VaultsPage />; break;
    case 'peers': content = <PeersPage />; break;
    case 'snipsPage': content = <SnippetsPage />; break;
    case 'rulesPage': content = <RulesPage />; break;
    case 'sftpPage': {
      const sf = sessions.find((x) => x.kind === 'sftp');
      content = sf ? <SftpView /> : <SftpPageEmpty />;
      break;
    }
    case 'sftp': content = <SftpView />; break;
    case 'screen': content = <RemoteView />; break;
    case 'terminal': content = <TerminalView />; break;
    default: content = <EmptyState />; break;
  }
  return <div className="stage" data-active={active ?? ''}>{content}</div>;
}

export default function App() {
  const nav = useAppStore((s) => s.nav);
  const collapsed = useAppStore((s) => s.collapsed);
  const focus = useAppStore((s) => s.focus);
  const showStatus = useAppStore((s) => s.showStatus);
  const booted = useAppStore((s) => s.booted);
  useTheme();
  useHotkeys();
  useContextMenu(); // 六类区域共用一个 contextmenu 委托监听
  useLang(); // 语言切换重渲染

  // 启动加载 + 事件订阅（卸载时全部 unlisten）
  // StrictMode 下 dev 会同步卸载→重挂载：首次 cleanup 时异步订阅可能尚未 resolve，
  // 若直接 push 进外层数组会漏掉 cleanup，导致监听器泄漏（term://data 双份 → 回显重复）。
  // 标准修法：await 全部订阅后先检查 disposed，已卸载则立即反注册自己。
  useEffect(() => {
    void useAppStore.getState().init();
    let disposed = false;
    const unlisteners: (() => void)[] = [];
    void (async () => {
      // 单个 listen 失败不能让其余订阅"注册了却永不清理"，更不能产生
      // unhandled rejection（那会导致终端/事件静默失效）。用 allSettled 收集
      // 成功项：卸载时统一反注册，失败项在 dev 下打印。
      const settled = await Promise.allSettled([
        listen<TermDataEvent>(EV.termData, (p) => writeTermData(p.sessionId, p.data)),
        listen<TermExitEvent>(EV.termExit, (p) => {
          useAppStore.getState().onConnState(p.sessionId, 'disconnected');
        }),
        listen<ConnStateEvent>(EV.connState, (p) => {
          // reason：后端区分「网络掉线 lost」与「用户主动断开 closed_by_user」，自动重连只认 lost
          useAppStore.getState().onConnState(p.sessionId, p.state, p.latencyMs, p.reason);
        }),
        listen<HostkeyChallengeEvent>(EV.hostkey, (p) => {
          useAppStore.getState().onHostkeyChallenge(p);
        }),
        listen<SftpProgressEvent>(EV.sftpProgress, (p) => {
          useAppStore.getState().onSftpProgress(p);
        }),
        listen<SftpChangedEvent>(EV.sftpChanged, () => {
          useAppStore.getState().onSftpChanged();
        }),
        listen<TunnelStateEvent>(EV.tunnelState, (p) => {
          // 转发监听运行态（重连/关闭会话时后端会补发 active:false）
          useAppStore.getState().onTunnelState(p);
        }),
        listen<MetricsSampleEvent>(EV.metrics, (p) => {
          useAppStore.getState().onMetricsSample(p);
        }),
        // 统一错误入口走 store action（toast + E_HOSTKEY_EXPIRED 时清 hostkey 弹窗）
        listen<AppErrorEvent>(EV.appError, (p) => {
          useAppStore.getState().onAppError(p);
        }),
        // 局域网对等层：发现列表变化 / 配对流程事件
        listen<DiscoveredDevice[]>(EV.peerDiscovered, (list) => {
          useAppStore.setState({ discovered: list });
        }),
        listen<PairEvent>(EV.peerPairing, (e) => {
          useAppStore.getState().onPeerPairing(e);
        }),
        // 配置收件箱：对端写入完成后推送（含摘要）
        listen<InboxNotice>(EV.peerInbox, (n) => {
          useAppStore.getState().onPeerInbox(n);
        }),
        // 终端控制：本机确认 / 横幅 / 拒绝提示
        listen<ControlRequest>(EV.peerControlRequest, (r) => {
          useAppStore.getState().onPeerControlRequest(r);
        }),
        listen<ControlSessionEvent>(EV.peerSession, (ev) => {
          useAppStore.getState().onPeerSession(ev);
        }),
        listen<PeerDeniedEvent>(EV.peerDenied, (ev) => {
          useAppStore.getState().onPeerDenied(ev);
        }),
        // 远程桌面：屏幕查看确认 / 被查看横幅
        listen<ScreenRequest>(EV.peerScreenRequest, (r) => {
          useAppStore.getState().onScreenRequest(r);
        }),
        listen<ScreenSessionEvent>(EV.peerScreenSession, (ev) => {
          useAppStore.getState().onScreenSession(ev);
        }),
        listen<InputStatsEvent>(EV.peerInputStats, (p) => {
          useAppStore.getState().onInputStats(p);
        }),
        listen<ClipboardEndedEvent>(EV.peerClipboard, (p) => {
          useAppStore.getState().onClipboardEnded(p);
        }),
      ]);
      const subs: (() => void)[] = [];
      settled.forEach((r) => {
        if (r.status === 'fulfilled') subs.push(r.value);
        else if (import.meta.env.DEV) console.warn('[event] subscribe failed:', r.reason);
      });
      if (disposed) { subs.forEach((u) => u()); return; }
      unlisteners.push(...subs);
    })();
    return () => { disposed = true; unlisteners.forEach((u) => u()); };
  }, []);

  const cls = 'app' + ((collapsed || nav === 'x') ? ' noside' : '') + (focus ? ' focus' : '');
  return (
    <div className={cls}>
      <TitleBar />
      <PeerControlBanner />
      <div className="bodyrow">
        <Sidebar />
        <Splitter />
        <section className="workspace">
          <TabBar />
          <Stage />
        </section>
      </div>
      {showStatus && <StatusBar />}
      {focus && (
        <button className="focusexit" title={T('focus.exit')} onClick={() => useAppStore.getState().toggleFocus()}>
          <Icon n="sidebar" s={13} />{T('focus.exit')}
        </button>
      )}
      {booted && <OverlayHost />}
      {booted && <PeerPairDialog />}
      {booted && <PeerControlDialog />}
      {booted && <PeerScreenDialog />}
      <EncodingPicker />
      <ToastHost />
    </div>
  );
}
