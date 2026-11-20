// SFTP 双栏视图：工具栏 + 本地/远程两个 FilePane + TransferQueue
// 拖拽传输（按下文件行拖到对侧/目录行）用 **Pointer 自绘**实现：Tauri 的
// dragDropEnabled 会屏蔽 WebView2 内的 HTML5 DnD（dragstart 永不触发，面板内
// 拖拽因此不会生效），Pointer 方案在桌面端与浏览器行为一致。
// 另有资源管理器拖入上传（Tauri drag-drop 事件，见下方 extDrop）。
import { useCallback, useEffect, useRef, useState } from 'react';
import type { PointerEvent as ReactPointerEvent } from 'react';
import { Button } from '../ui/Button';
import { useAppStore } from '../../store/useAppStore';
import { FilePane } from './FilePane';
import { TransferQueue } from './TransferQueue';
import { hostById, selectSftpSessionId } from '../../store/selectors';
import { isTauri } from '../../ipc/env';
import { ConnectingView, TerminalBanner } from '../terminal/TerminalView';
import { T, useLang } from '../../i18n';

interface DragInfo { side: 'local' | 'remote'; name: string }
interface DropInfo {
  side: 'local' | 'remote';
  text: string;
  rect: { left: number; top: number; width: number; height: number };
  row: boolean;
  dirPath?: string;
  rowName?: string;
}

export function SftpView() {
  useLang();
  const localPath = useAppStore((s) => s.localPath);
  const remotePath = useAppStore((s) => s.remotePath);
  const selLocal = useAppStore((s) => s.selLocal);
  const selRemote = useAppStore((s) => s.selRemote);
  const rev = useAppStore((s) => s.sftpRev);
  const dispatch = useAppStore((s) => s.dispatch);
  const sessions = useAppStore((s) => s.sessions);
  const active = useAppStore((s) => s.active);
  // 会话解析统一走 selectSftpSessionId（active 优先 → 第一个 sftp → 第一个 term），
  // 与 FilePane 的列表加载 / 写操作完全同一规则；解析出的会话用于标题与连接状态展示。
  const sftpSessionId = selectSftpSessionId({ sessions, active });
  const s = sftpSessionId ? sessions.find((x) => x.id === sftpSessionId) ?? null : null;
  const ensureRemotePath = useAppStore((st) => st.ensureRemotePath);
  // 首次进入（或换主机）时确定远端起始目录：终端 cwd → 上次路径 → 服务器 home。
  useEffect(() => {
    void ensureRemotePath();
  }, [ensureRemotePath, s?.id, remotePath]);
  // 终端里 cd 之后（OSC 7 上报），SFTP 也跟随到新目录。
  // 只在 cwd **真正变化**时同步——重挂载（终端↔SFTP 切视图）不能用手选目录
  // 之前的旧 cwd 覆盖用户当前浏览位置。
  const cwdOfSession = useAppStore((st) => (s ? st.cwdBySession[s.id] : undefined));
  const lastAppliedCwd = useRef<string | null>(null);
  useEffect(() => {
    if (!cwdOfSession || cwdOfSession === lastAppliedCwd.current) return;
    lastAppliedCwd.current = cwdOfSession;
    useAppStore.getState().setSftpPath('remote', cwdOfSession);
  }, [cwdOfSession]);
  const hostName = s ? (hostById(s.hostId)?.name ?? s.title.replace(' · SFTP', '')) : T('mk.remote');
  const canUp = !!selLocal;
  const canDown = !!selRemote;
  const [drag, setDrag] = useState<DragInfo | null>(null);
  const [hover, setHover] = useState<DropInfo | null>(null);

  // 窄窗单栏：容器 <560px 时同一时刻只渲染一侧（双栏各 ~150px，文件名/权限都读不出来），
  // 顶部提供「本机 / 远端」切换；宽度恢复后自动回到双栏。
  const [narrow, setNarrow] = useState(false);
  const [narrowSide, setNarrowSide] = useState<'local' | 'remote'>('remote');
  const narrowSideRef = useRef(narrowSide);
  narrowSideRef.current = narrowSide;
  const roRef = useRef<ResizeObserver | null>(null);
  // 用回调 ref 而不是 useEffect：连接中会先渲染 ConnectingView（此时还没有 .sftp 节点），
  // 一次性 effect 会错过节点出现，导致宽度已经 <560 却仍是双栏。
  const attachSftp = useCallback((el: HTMLDivElement | null) => {
    roRef.current?.disconnect();
    roRef.current = null;
    if (!el) return;
    const sync = () => setNarrow(el.clientWidth > 0 && el.clientWidth < 560);
    sync();
    const ro = new ResizeObserver(sync);
    ro.observe(el);
    roRef.current = ro;
  }, []);

  // ---- 指针自绘拖拽（面板内文件行 → 对侧面板/目录行 = 上传/下载）----
  const clearDnd = () => { setDrag(null); setHover(null); };
  interface FileDragState {
    side: 'local' | 'remote'; name: string;
    x0: number; y0: number; pointerId: number;
    started: boolean; ghost: HTMLDivElement | null;
    /** 单栏时拖到顶部切换器上的悬停计时（500ms 后自动切到对侧） */
    switchSide: 'local' | 'remote' | null;
    switchTimer: number | null;
  }
  const fileDragRef = useRef<FileDragState | null>(null);
  // hover 的镜像：pointerup 处理器需要读到最新落点（state 在闭包里会陈旧）
  const hoverRef = useRef<DropInfo | null>(null);
  hoverRef.current = hover;
  const suppressUntil = useRef(0);

  /** 拖拽中按坐标刷新落点高亮（对侧面板整块 / 目录行）。 */
  const paintFileTarget = (side: 'local' | 'remote', x: number, y: number) => {
    const el = document.elementFromPoint(x, y) as HTMLElement | null;
    const pane = el?.closest('.pane') as HTMLElement | null;
    const targetSide = pane?.getAttribute('data-side') as 'local' | 'remote' | undefined;
    if (!pane || !targetSide || targetSide === side) { setHover(null); return; }
    const dirRow = el!.closest('.frow.dir') as HTMLElement | null;
    if (dirRow && pane.contains(dirRow)) {
      const r = dirRow.getBoundingClientRect();
      const dirPath = dirRow.dataset.dirPath;
      setHover((prev) => (prev && prev.side === targetSide && prev.row && prev.dirPath === dirPath
        ? prev
        : {
          side: targetSide, row: true, dirPath, rowName: dirRow.dataset.name, text: '',
          rect: { left: r.left, top: r.top, width: r.width, height: r.height },
        }));
      return;
    }
    const r = pane.getBoundingClientRect();
    const text = side === 'local' ? T('sftp.dropUp') : T('sftp.dropDown');
    setHover((prev) => (prev && prev.side === targetSide && !prev.row
      ? prev
      : { side: targetSide, row: false, text, rect: { left: r.left, top: r.top, width: r.width, height: r.height } }));
  };

  /** 结束拖拽：drop=true 且落点有效 → 触发上传/下载；总是清理现场。 */
  const endFileDrag = (drop: boolean) => {
    const a = fileDragRef.current;
    if (!a) return;
    const h = hoverRef.current;
    clearSwitchTimer(a);
    if (a.ghost) a.ghost.remove();
    document.body.classList.remove('host-dragging');
    window.removeEventListener('pointermove', onFileDragMove);
    window.removeEventListener('pointerup', onFileDragUp);
    window.removeEventListener('keydown', onFileDragKey, true);
    window.removeEventListener('blur', onFileDragCancel);
    fileDragRef.current = null;
    if (!a.started) return;
    suppressUntil.current = Date.now() + 400;   // 吞掉拖拽尾随的 click（不误选文件）
    if (drop && h && h.side !== a.side) {
      void useAppStore.getState().transferDrop(a.side, a.name, h.row ? h.dirPath : undefined);
    }
    clearDnd();
  };

  /** 单栏拖拽辅助：指针落在顶部「本机 / 远端」切换器上时返回目标侧。
      拖拽 ghost 是 pointer-events:none，elementFromPoint 拿到的是下面的按钮。 */
  const switchSideAt = (x: number, y: number): 'local' | 'remote' | null => {
    const el = document.elementFromPoint(x, y) as HTMLElement | null;
    const btn = el?.closest('[data-sftp-switch] button') as HTMLElement | null;
    const side = btn?.dataset.side;
    return side === 'local' || side === 'remote' ? side : null;
  };
  const clearSwitchTimer = (a: FileDragState) => {
    if (a.switchTimer != null) { window.clearTimeout(a.switchTimer); a.switchTimer = null; }
    a.switchSide = null;
    document.querySelectorAll('.pane-switch button.armed').forEach((b) => b.classList.remove('armed'));
  };

  const onFileDragMove = (ev: PointerEvent) => {
    const a = fileDragRef.current;
    if (!a || ev.pointerId !== a.pointerId) return;
    // 指针已无按键（漏掉 pointerup，如窗口外松手）：按取消收尾
    if (ev.buttons === 0) { endFileDrag(false); return; }
    if (!a.started) {
      if (Math.hypot(ev.clientX - a.x0, ev.clientY - a.y0) < 6) return;
      a.started = true;
      document.body.classList.add('host-dragging');   // 复用抓取态光标/禁选中样式
      const ghost = document.createElement('div');
      ghost.className = 'hostdrag-ghost';
      const nameEl = document.createElement('span');
      nameEl.className = 'hd-name';
      nameEl.textContent = a.name;
      ghost.append(nameEl);
      document.body.appendChild(ghost);
      a.ghost = ghost;
      setDrag({ side: a.side, name: a.name });
    }
    ev.preventDefault();
    if (a.ghost) a.ghost.style.transform = `translate3d(${ev.clientX + 12}px, ${ev.clientY + 10}px, 0)`;
    paintFileTarget(a.side, ev.clientX, ev.clientY);

    // 单栏辅助：拖到顶部「本机 / 远端」切换器上停留 500ms，自动切到对侧，继续拖进面板即可放下。
    if (!narrow) return;
    const over = switchSideAt(ev.clientX, ev.clientY);
    if (over && over !== narrowSideRef.current) {
      if (a.switchSide !== over) {
        clearSwitchTimer(a);
        a.switchSide = over;
        document.querySelector(`[data-sftp-switch] button[data-side="${over}"]`)?.classList.add('armed');
        a.switchTimer = window.setTimeout(() => {
          a.switchTimer = null;
          a.switchSide = null;
          document.querySelectorAll('.pane-switch button.armed').forEach((b) => b.classList.remove('armed'));
          narrowSideRef.current = over;
          setNarrowSide(over);
          // 切换后等新面板挂载完再按当前指针位置重算落点
          requestAnimationFrame(() => paintFileTarget(a.side, ev.clientX, ev.clientY));
        }, 500);
      }
    } else if (a.switchSide) {
      clearSwitchTimer(a);
    }
  };

  const onFileDragUp = (ev: PointerEvent) => {
    const a = fileDragRef.current;
    if (!a || ev.pointerId !== a.pointerId) return;
    if (a.started) paintFileTarget(a.side, ev.clientX, ev.clientY);  // 松手处再算一次
    endFileDrag(true);
  };

  const onFileDragKey = (ev: KeyboardEvent) => {
    if (ev.key !== 'Escape' || !fileDragRef.current) return;
    ev.preventDefault();
    ev.stopPropagation();
    endFileDrag(false);
  };
  const onFileDragCancel = () => endFileDrag(false);

  /** 文件行 pointerdown 入口：只跟踪文件（目录不可拖），超过 6px 才算拖拽（不干扰单击选中）。 */
  const onRowPointerDown = (side: 'local' | 'remote', ev: ReactPointerEvent, name: string, isDir: boolean) => {
    if (ev.button !== 0 || isDir) return;
    const t = ev.target as HTMLElement | null;
    if (t?.closest('button,a,input,textarea,select')) return;
    if (fileDragRef.current) return;
    fileDragRef.current = {
      side, name,
      x0: ev.clientX, y0: ev.clientY, pointerId: ev.pointerId,
      started: false, ghost: null, switchSide: null, switchTimer: null,
    };
    // pointer capture：拖出窗口松手也能收到 pointerup（不留残留拖拽态）
    try { (ev.currentTarget as HTMLElement).setPointerCapture?.(ev.pointerId); } catch { /* ignore */ }
    window.addEventListener('pointermove', onFileDragMove);
    window.addEventListener('pointerup', onFileDragUp);
    window.addEventListener('keydown', onFileDragKey, true);
    window.addEventListener('blur', onFileDragCancel);
  };
  const consumeClickSuppress = () => Date.now() < suppressUntil.current
    ? (suppressUntil.current = 0, true)
    : false;

  // 卸载兜底清理（拖拽进行中切视图等）
  useEffect(() => () => endFileDrag(false), []);

  // ---- 资源管理器拖入文件直接上传（Tauri drag-drop）----
  // HTML5 拖拽（面板内部互拖）不会触发 Tauri 的 drag-drop 事件，两者互不干扰。
  // 命中规则：drop 位置落在远端面板 → 上传到当前目录（落在目录行则上传到该目录）。
  type ExtDrop = { left: number; top: number; width: number; height: number; dirPath?: string };
  const [extDrop, setExtDrop] = useState<ExtDrop | null>(null);
  useEffect(() => {
    if (!isTauri) return;
    let unlisten: (() => void) | null = null;
    let disposed = false;
    void (async () => {
      try {
        const { getCurrentWebview } = await import('@tauri-apps/api/webview');
        const off = await getCurrentWebview().onDragDropEvent((ev) => {
          if (disposed) return;
          if (ev.payload.type === 'leave') { setExtDrop(null); return; }
          // 物理像素 → CSS 逻辑像素
          const dpr = window.devicePixelRatio || 1;
          const lx = ev.payload.position.x / dpr;
          const ly = ev.payload.position.y / dpr;
          // 只在已连接的 SFTP 视图上接管
          const st = useAppStore.getState();
          const sid = selectSftpSessionId({ sessions: st.sessions, active: st.active });
          const sess = sid ? st.sessions.find((x) => x.id === sid) : null;
          const el = document.elementFromPoint(lx, ly) as HTMLElement | null;
          const paneSide = el?.closest('.pane')?.getAttribute('data-side');
          if (ev.payload.type === 'drop') {
            const dirRow = el?.closest('.frow.dir') as HTMLElement | null;
            const dirPath = dirRow?.dataset.dirPath;
            setExtDrop(null);
            if (paneSide === 'remote' && sess && sess.status === 'connected' && ev.payload.paths.length) {
              void useAppStore.getState().uploadDroppedFiles(ev.payload.paths, dirPath);
            }
            return;
          }
          // enter / over：远端面板上给出放置提示
          if (paneSide === 'remote' && sess && sess.status === 'connected') {
            const pane = el!.closest('.pane') as HTMLElement;
            const r = pane.getBoundingClientRect();
            setExtDrop((prev) => {
              const next = { left: r.left, top: r.top, width: r.width, height: r.height };
              return prev ? prev : next;
            });
          } else {
            setExtDrop(null);
          }
        });
        if (disposed) off();
        else unlisten = off;
      } catch { /* 非 Tauri / API 不可用：静默降级 */ }
    })();
    return () => { disposed = true; unlisten?.(); };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  if (s?.status === 'connecting') {
    return <ConnectingView title={hostName} onCancel={() => useAppStore.getState().closeSession(s.id)} />;
  }

  return (
    <div className={'sftp' + (narrow ? ' narrow' : '')} ref={attachSftp}>
      {s?.status === 'disconnected' && (
        <TerminalBanner sessionId={s.id} title={hostName} error={s.error} />
      )}
      <div className="toolbar">
        {narrow && (
          <div className="pane-switch" data-sftp-switch>
            <button data-side="local" className={narrowSide === 'local' ? 'on' : ''} onClick={() => setNarrowSide('local')}>{T('sftp.local')}</button>
            <button data-side="remote" className={narrowSide === 'remote' ? 'on' : ''} onClick={() => setNarrowSide('remote')}>{T('sftp.remote')}</button>
          </div>
        )}
        <span className="tt">{hostName} · SFTP</span>
        <span className="chip">{T(s?.status === 'disconnected' ? 'side.disconnected' : 'sftp.connected')}</span>
        <span className="sp" />
        <Button icon="upload" primary={canUp} disabled={!canUp} title={T('sftp.upload')} onClick={() => dispatch('upload')}>
          <span className="tbtn-label">{T('sftp.upload')}</span>
        </Button>
        <Button icon="download" primary={canDown} disabled={!canDown} title={T('sftp.download')} onClick={() => dispatch('download')}>
          <span className="tbtn-label">{T('sftp.download')}</span>
        </Button>
        <span className="sep" />
        <Button icon="refresh" title={T('sftp.refresh')} onClick={() => dispatch('frefresh', 'both')} />
        <Button icon="trash" title={T('sftp.deleteSel')} onClick={() => dispatch('fdelete')} />
      </div>
      <div className="sftppanes">
        {(!narrow || narrowSide === 'local') && (
          <FilePane side="local" title={T('sftp.local')} path={localPath} selected={selLocal} rev={rev}
            onRowPointerDown={onRowPointerDown} clickSuppressed={consumeClickSuppress}
            dragging={drag?.side === 'local' ? drag.name : null}
            dropActive={hover?.side === 'local' && !hover.row}
            dirOver={hover?.side === 'local' && hover.row ? hover.rowName ?? null : null} />
        )}
        {(!narrow || narrowSide === 'remote') && (
          <FilePane side="remote" title={hostName} path={remotePath} selected={selRemote} rev={rev}
            onRowPointerDown={onRowPointerDown} clickSuppressed={consumeClickSuppress}
            dragging={drag?.side === 'remote' ? drag.name : null}
            dropActive={hover?.side === 'remote' && !hover.row}
            dirOver={hover?.side === 'remote' && hover.row ? hover.rowName ?? null : null} />
        )}
      </div>
      <TransferQueue />
      {hover && (
        <div
          className={'dropzone on' + (hover.row ? ' row' : '')}
          style={{
            left: hover.rect.left, top: hover.rect.top,
            width: hover.rect.width, height: hover.rect.height,
          }}
        >
          {!hover.row && <div className="droptip">{hover.text}</div>}
        </div>
      )}
      {extDrop && (
        <div className="dropzone on" style={{ left: extDrop.left, top: extDrop.top, width: extDrop.width, height: extDrop.height }}>
          <div className="droptip">{T('sftp.dropFilesUp')}</div>
        </div>
      )}
    </div>
  );
}
