// xterm 挂载点 + 连接中 / 断线横幅（连接生命周期）
// 断线自动重连期间标签保持 connecting 样式，终端不卸载（保留 scrollback），
// 横幅显示「将于 Ns 后重连（k/5）」；终端查找条锚定在右上角。
import { useEffect, useRef, useState } from 'react';
import { attachTerm, getTermViewport, subscribeTermRender } from './useXterm';
import { useAppStore } from '../../store/useAppStore';
import { activeSession, hostById } from '../../store/selectors';
import { hhmm } from '../../lib/format';
import { MAX_RECONNECT_ATTEMPTS } from '../../lib/reconnect';
import { Icon } from '../ui/Icon';
import { TermFindBar } from './TermFindBar';
import { T, useLang } from '../../i18n';

export function TerminalBanner({ sessionId, title, error }: { sessionId: string; title: string; error?: string }) {
  useLang();
  const reconnect = useAppStore((s) => s.reconnect);
  const dispatch = useAppStore((s) => s.dispatch);
  // 断线时刻：显示「最后响应」必须取断线时刻，否则任意重渲染都会漂移到当前时间。
  // disconnectedAt 缺失（历史会话 / 字段未落地）时传 undefined → hhmm() 回退当前时刻。
  const disconnectedAt = useAppStore((s) => s.sessions.find((x) => x.id === sessionId)?.disconnectedAt);
  return (
    <div className="banner" data-error={error ? '1' : undefined}>
      <Icon n="alert" s={14} />
      <span>{T('term.disconnected', { 0: title, 1: hhmm(disconnectedAt) })}</span>
      {error && <span className="berr">{error}</span>}
      <span className="sp" />
      <button onClick={() => void reconnect(sessionId)}>{T('term.reconnect')}</button>
      <button className="ghost" onClick={() => dispatch('closedisc', sessionId)}>{T('term.closeTab')}</button>
    </div>
  );
}

/** 自动重连倒计时横幅：可「立即重连」或取消重连回到断线终态 */
export function RetryBanner({ sessionId, attempt, remain }: { sessionId: string; attempt: number; remain: number }) {
  useLang();
  const retryNow = useAppStore((s) => s.retryNow);
  const cancel = useAppStore((s) => s.cancelAutoReconnect);
  return (
    <div className="banner" data-retry="1">
      <Icon n="refresh" s={14} />
      <span data-find="retry">{T('term.retryWait', { 0: remain, 1: attempt, 2: MAX_RECONNECT_ATTEMPTS })}</span>
      <span className="sp" />
      <button onClick={() => retryNow(sessionId)}>{T('term.retryNow')}</button>
      <button className="ghost" onClick={() => cancel(sessionId)}>{T('term.retryCancel')}</button>
    </div>
  );
}

export function ConnectingView({ title, onCancel }: { title: string; onCancel: () => void }) {
  useLang();
  return (
    <div className="connwait">
      <span className="connspin" />
      <div>{T('term.connecting', { 0: title })}</div>
      <button className="tbtn" onClick={onCancel}>{T('c.cancel')}</button>
    </div>
  );
}

export function TerminalView() {
  const s = activeSession();
  const findOpen = useAppStore((st) => st.findOpen);
  const retry = useAppStore((st) => (st.active ? st.retry[st.active] : undefined));
  const ref = useRef<HTMLDivElement | null>(null);
  // 必须在任何提前 return 之前调用所有 hooks：connecting → connected 切换时
  // 若 hook 数量变化会触发 React #310（白屏）。
  const showLines = useAppStore((st) => st.showLineNumbers);
  const retrying = !!s && s.status === 'connecting' && !!s.retrying;
  useEffect(() => {
    if (!s || s.kind === 'sftp' || !ref.current) return;
    // 普通连接中不挂终端（数据先缓冲）；自动重连中必须保持挂载，保留 scrollback
    if (s.status === 'connecting' && !s.retrying) return;
    const detach = attachTerm(s.id, ref.current);
    return detach;
    // showLines 变化会改变 .termhost 的兄弟结构，必须重新 attach，
    // 否则 xterm 的 DOM 会随旧节点一起被丢弃（表现为终端空白）。
  }, [s?.id, s?.status, s?.retrying, showLines]);
  if (!s || s.kind === 'sftp') return null;
  const title = s.hostId ? (hostById(s.hostId)?.name ?? s.title.split(' · ')[0]) : T('app.localTerm');
  if (s.status === 'connecting' && !retrying) {
    // 占位期间也保留 xterm 容器（隐藏、仍有布局尺寸）；数据由 useXterm 的 256 KiB
    // pendingData 缓冲兜底，连接成功后 attach 时回放，避免 cmd / SSH 首屏输出丢失。
    return (
      <div className="termwrap" data-status="connecting">
        <ConnectingView title={title} onCancel={() => useAppStore.getState().closeSession(s.id)} />
        <div ref={ref} className="termhost connholder" aria-hidden="true" />
      </div>
    );
  }
  const banner = s.status === 'disconnected' || retrying;
  return (
    <div
      className="termwrap"
      data-status={s.status}
      data-ctx="term"
      data-lineno={showLines ? '1' : '0'}
      data-retrying={retrying ? '1' : undefined}
      data-banner={banner ? '1' : undefined}
    >
      {s.status === 'disconnected' && <TerminalBanner sessionId={s.id} title={title} error={s.error} />}
      {retrying && retry && <RetryBanner sessionId={s.id} attempt={retry.attempt} remain={retry.remain} />}
      <div className={`termbody${showLines ? ' has-gutter' : ''}`}>
        {showLines && <TermGutter key="gutter" sessionId={s.id} />}
        <div key="host" ref={ref} className="termhost" />
      </div>
      {findOpen && <TermFindBar key={s.id} sessionId={s.id} />}
    </div>
  );
}

/** 终端行号栏：独立 gutter 层，不写入 xterm 缓冲区（复制不会带号）。
 *  只给输出行编号，正在输入的光标行不编号；跟随内容/滚动/尺寸变化更新。 */
function TermGutter({ sessionId }: { sessionId: string }) {
  // 行号栏的几何完全由「实时单元格高度 + 内容顶部偏移」决定：
  // 用绝对定位逐行落点，改字号/行高/字体后不会因为 flex 高度滞后而越往下越错位。
  const [view, setView] = useState<{ numbers: (number | null)[]; cellH: number; padTop: number } | null>(null);
  const ml = useAppStore((st) => st.lineHeight);
  const mf = useAppStore((st) => st.fontSize);
  const mfont = useAppStore((st) => st.font);
  useEffect(() => {
    let alive = true;
    // 终端每帧都会 onRender（120Hz 下每秒 120 次）：只有「编号序列或几何」真的变了
    // 才 setState，否则会让 React 每帧重复 reconcile 几十个行号节点，白白吃 CPU。
    let lastSig = '';
    const sync = () => {
      if (!alive) return;
      const next = getTermViewport(sessionId);
      const sig = next ? `${next.cellH.toFixed(2)}|${next.padTop.toFixed(2)}|${next.numbers.join(',')}` : '';
      if (sig === lastSig) return;
      lastSig = sig;
      setView(next);
    };
    sync();
    const off = subscribeTermRender(sessionId, sync);
    // 兜底：字体/尺寸变化后 xterm 的重排回调可能早于我们的 setState，
    // 这里在下一帧、以及 120ms 后再同步一次，保证行号跟上新网格。
    const raf = requestAnimationFrame(sync);
    const t1 = setTimeout(sync, 120);
    const t2 = setInterval(sync, 400);
    return () => { alive = false; off(); cancelAnimationFrame(raf); clearTimeout(t1); clearInterval(t2); };
  }, [sessionId, ml, mf, mfont]);
  const cellH = view?.cellH || 0;
  const padTop = view?.padTop || 0;
  return (
    <div className="termgutter" aria-hidden="true">
      {(view?.numbers ?? []).map((n, i) => (
        n === null ? null : (
          <div
            className="tln"
            key={i}
            style={{ position: 'absolute', top: padTop + i * cellH, height: cellH, lineHeight: `${cellH}px` }}
          >{n}</div>
        )
      ))}
    </div>
  );
}
