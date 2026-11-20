// 连接信息面板：
// 打开 → start_metrics；关闭/切换 overlay → 卸载时 stop_metrics；
// metrics://sample 按 sessionId 过滤，sparkline 保留最近 24 帧；颜色全走 CSS 变量。
import { useEffect, useRef } from 'react';
import { Sheet } from '../overlays/Sheet';
import { Icon } from '../ui/Icon';
import { Button } from '../ui/Button';
import { useAppStore } from '../../store/useAppStore';
import { hostById } from '../../store/selectors';
import { T, useLang } from '../../i18n';
import * as api from '../../ipc/commands';
import type { HostMetrics } from '../../types/models';

const POINTS = 24;

/** 纯 SVG polyline sparkline（24 点，高 20px） */
function Sparkline({ data, cls }: { data: number[]; cls?: string }) {
  const pts = data.slice(-POINTS);
  while (pts.length < POINTS) pts.unshift(pts[0] ?? 0);
  const max = Math.max(...pts, 0.0001);
  const step = 100 / (POINTS - 1);
  const line = pts.map((v, i) => `${(i * step).toFixed(2)},${(18 - (v / max) * 16).toFixed(2)}`).join(' ');
  return (
    <svg className={'spark' + (cls ? ' ' + cls : '')} viewBox="0 0 100 20" preserveAspectRatio="none" aria-hidden="true">
      <polyline points={line} fill="none" stroke="currentColor" strokeWidth="1.4" vectorEffect="non-scaling-stroke" />
    </svg>
  );
}

function pct(a: number, b: number): number { return b > 0 ? Math.min(100, Math.round((a / b) * 100)) : 0; }
function f1(v: number): string { return v.toFixed(1); }

function MetricBar({ p }: { p: number }) {
  const cls = p >= 90 ? 'high' : p >= 70 ? 'warn' : '';
  // 用 --p（0..1）驱动 scaleX，避免 width 过渡逐帧触发布局（高刷适配）
  return (
    <div className="mbar">
      <i className={cls} style={{ ['--p' as string]: Math.max(0, Math.min(1, p / 100)) } as React.CSSProperties} />
    </div>
  );
}

export function ConnInfoSheet() {
  useLang();
  const metric = useAppStore((s) => (s.active ? s.metrics[s.active] : undefined));
  const metricsLive = useAppStore((s) => s.metricsLive);
  const sessions = useAppStore((s) => s.sessions);
  const activeId = useAppStore((s) => s.active);
  const dispatch = useAppStore((s) => s.dispatch);
  const s = sessions.find((x) => x.id === activeId) ?? null;
  const host = s && s.hostId ? hostById(s.hostId) : undefined;
  const title = s ? (host?.name ?? (s.kind === 'local' ? T('app.localTerm') : s.title)) : '';

  // 趋势线：本地保留最近 24 帧（按 sessionId 切换时重置）
  const hist = useRef<{ cpu: number[]; mem: number[]; rx: number[]; tx: number[]; rd: number[]; wr: number[] }>(
    { cpu: [], mem: [], rx: [], tx: [], rd: [], wr: [] },
  );
  const lastSid = useRef<string | null>(null);
  const lastAt = useRef<number>(0);
  if (metric) {
    if (lastSid.current !== metric.sessionId) {
      hist.current = { cpu: [], mem: [], rx: [], tx: [], rd: [], wr: [] };
      lastSid.current = metric.sessionId;
      lastAt.current = 0;
    }
    if (metric.sampledAt !== lastAt.current) {
      lastAt.current = metric.sampledAt;
      const h = hist.current;
      const push = (a: number[], v: number) => { a.push(v); if (a.length > POINTS) a.shift(); };
      push(h.cpu, metric.cpuPct); push(h.mem, metric.memUsedGb);
      push(h.rx, metric.netRxMbps); push(h.tx, metric.netTxMbps);
      push(h.rd, metric.diskReadMbps); push(h.wr, metric.diskWriteMbps);
    }
  }

  // 打开 → start_metrics；关闭/切 overlay（卸载）→ stop_metrics，不留后台 task。
  // 本地终端没有远端指标：不采集、不伪造。
  useEffect(() => {
    // 暂停时既不启动采样，也不因切标签重新 start
    if (!s || s.status !== 'connected' || s.kind === 'local' || !metricsLive) return;
    void api.startMetrics(s.id).catch(() => { /* 断线/不支持：面板显示断开态 */ });
    return () => { void api.stopMetrics(s.id).catch(() => { /* ignore */ }); };
    // 切换标签立即切换数据；metricsLive 变化时暂停 / 恢复采样
  }, [s?.id, s?.status, s?.kind, metricsLive]);

  /* ---------- 无活动会话：空态，不渲染 0 值假指标卡 ---------- */
  if (!s) {
    return (
      <Sheet title={T('ci.title')}>
        <div className="mempty">
          {T('ci.empty')}
          <br />
          {T('ci.emptyHint')}
          <div className="erow" style={{ justifyContent: 'center', marginTop: 14 }}>
            <Button icon="plus" primary onClick={() => dispatch('hostform')}>{T('ci.newConn')}</Button>
            <Button icon="box" onClick={() => { dispatch('closeov'); dispatch('goto', 'hosts'); }}>{T('ci.toHosts')}</Button>
          </div>
        </div>
      </Sheet>
    );
  }

  const disc = s.status === 'disconnected';
  const waiting = s.status === 'connecting';
  const m: HostMetrics | undefined = metric;

  /* ---------- 本地终端：没有远端指标，不渲染 0 值假卡片 ---------- */
  if (s.kind === 'local') {
    return (
      <Sheet title={T('ci.title')}>
        <div className="mempty">
          {T('ci.localEmpty')}
          <br />
          {T('ci.emptyHint')}
        </div>
      </Sheet>
    );
  }

  const h = hist.current;
  const cpuP = m ? Math.round(m.cpuPct) : 0;
  const memP = m ? pct(m.memUsedGb, m.memTotalGb) : 0;
  const diskP = m ? pct(m.diskUsedGb, m.diskTotalGb) : 0;
  const hhmm = new Date(s.startedAt).toLocaleTimeString(undefined, { hour12: false, hour: '2-digit', minute: '2-digit' });

  return (
    <Sheet title={T('ci.title')}>
      {/* 头部：主机名 / user@addr:port / 状态 */}
      <div className="mhero">
        <div className="mhico"><Icon n="terminal" s={18} /></div>
        <div className="mht">
          <h4>{title}</h4>
          <p>{host ? `${host.user}@${host.addr}:${host.port}` : s.title} · {T(s.kind === 'sftp' ? 'ci.sftpSession' : 'ci.sshSession')}</p>
        </div>
        <span className="mhstat" style={{ color: disc ? 'var(--red)' : waiting ? 'var(--orange)' : 'var(--green)' }}>
          <span className="dot" style={{ background: disc ? 'var(--red)' : waiting ? 'var(--orange)' : 'var(--green)', width: 6, height: 6 }} />
          {T(waiting ? 'side.connecting' : disc ? 'ci.disconnected' : 'ci.connected')}
        </span>
      </div>

      {(disc || waiting) ? (
        <div className="mempty">
          {waiting ? T('term.connecting', { 0: title }) : T('ci.discNote')}
          <div className="erow" style={{ justifyContent: 'center', marginTop: 14 }}>
            {disc
              ? <Button icon="refresh" primary onClick={() => dispatch('reconnect', s.id)}>{T('ci.resume')}</Button>
              : <Button icon="x" onClick={() => void useAppStore.getState().closeSession(s.id)}>{T('c.cancel')}</Button>}
          </div>
        </div>
      ) : (
        <>
          <div className="mtoolbar">
            <span className="mlive">
              <span className="dot on" style={{ width: 6, height: 6, animation: 'pulse 1.4s infinite' }} />
              {metricsLive ? T('ci.live', { 0: hhmm }) : T('ci.paused')}
            </span>
            <span className="sp" style={{ flex: 1 }} />
            <button className="tbtn" title={metricsLive ? T('ci.pause') : T('ci.resume2')} onClick={() => dispatch('metricstoggle')}>
              <Icon n={metricsLive ? 'pause' : 'play'} s={12} />{metricsLive ? T('ci.pause') : T('ci.resume2')}
            </button>
            <button className="tbtn" title={T('ci.refreshTitle')} onClick={() => dispatch('metricsrefresh')}>
              <Icon n="refresh" s={12} />{T('ci.refresh')}
            </button>
          </div>

          {!m ? (
            <div className="mempty">{T('ci.noMetrics')}</div>
          ) : (
            <div className="mgrid">
              {/* CPU */}
              <div className="mcard full">
                <div className="mtop"><Icon n="cpu" s={13} />{T('ci.cpu')}<span className="unit">{T('ci.cpuSub', { 0: m.cores, 1: f1(m.loadAvg[0]), 2: f1(m.loadAvg[1]) })}</span></div>
                <div className="mval">{Math.round(m.cpuPct)}<small>%</small></div>
                <MetricBar p={cpuP} />
                <Sparkline data={h.cpu} />
              </div>
              {/* 内存 */}
              <div className="mcard">
                <div className="mtop"><Icon n="mem" s={13} />{T('ci.mem')}<span className="unit">{f1(m.memUsedGb)} / {f1(m.memTotalGb)} GB</span></div>
                <div className="mval">{memP}<small>%</small></div>
                <MetricBar p={memP} />
                <Sparkline data={h.mem} cls="green" />
                <div className="msub">{T('ci.memSub', { 0: memP, 1: f1(m.memTotalGb - m.memUsedGb) })}</div>
              </div>
              {/* 存储 */}
              <div className="mcard">
                <div className="mtop"><Icon n="disk" s={13} />{T('ci.disk')}<span className="unit">{f1(m.diskUsedGb)} / {f1(m.diskTotalGb)} GB</span></div>
                <div className="mval">{diskP}<small>%</small></div>
                <MetricBar p={diskP} />
                <div className="msub">{T('ci.diskSub', { 0: Math.round(m.diskUsedGb), 1: Math.round(m.diskTotalGb) })}</div>
              </div>
              {/* 网络 */}
              <div className="mcard full">
                <div className="mtop"><Icon n="wifi" s={13} />{T('ci.net')}<span className="unit">MB/s</span></div>
                <div className="mrow">
                  <div className="mhalf">
                    <div className="mhlbl">{T('ci.down')}</div>
                    <div className="mhval">{f1(m.netRxMbps)}<small>MB/s</small></div>
                    <Sparkline data={h.rx} />
                  </div>
                  <div className="mhalf">
                    <div className="mhlbl">{T('ci.up')}</div>
                    <div className="mhval">{f1(m.netTxMbps)}<small>MB/s</small></div>
                    <Sparkline data={h.tx} cls="purple" />
                  </div>
                </div>

              </div>
              {/* 磁盘读写 */}
              <div className="mcard full">
                <div className="mtop"><Icon n="db" s={13} />{T('ci.diskio')}<span className="unit">MB/s</span></div>
                <div className="mrow">
                  <div className="mhalf">
                    <div className="mhlbl">{T('ci.read')}</div>
                    <div className="mhval">{f1(m.diskReadMbps)}<small>MB/s</small></div>
                    <Sparkline data={h.rd} />
                  </div>
                  <div className="mhalf">
                    <div className="mhlbl">{T('ci.write')}</div>
                    <div className="mhval">{f1(m.diskWriteMbps)}<small>MB/s</small></div>
                    <Sparkline data={h.wr} cls="purple" />
                  </div>
                </div>

              </div>
            </div>
          )}
        </>
      )}
    </Sheet>
  );
}
