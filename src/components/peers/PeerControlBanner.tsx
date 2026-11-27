// 本机被控制 / 被查看 / 被采集麦克风的常驻横幅：显示对端设备、来源、尺寸/画质与时长，可一键断开。
// 数据由 `peer://session` / `peer://screen-session` / `peer://mic-state` 事件维护；计时在前端本地每秒刷新。
import { useEffect, useState } from 'react';
import { useAppStore } from '../../store/useAppStore';
import { T, useLang } from '../../i18n';
import { Icon } from '../ui/Icon';

export function PeerControlBanner() {
  useLang();
  const controlled = useAppStore((s) => s.controlled);
  const viewed = useAppStore((s) => s.viewed);
  const micActive = useAppStore((s) => s.micActive);
  const refreshControl = useAppStore((s) => s.refreshControlled);
  const refreshViewed = useAppStore((s) => s.refreshViewed);
  const refreshMic = useAppStore((s) => s.refreshMicActive);
  const kickControl = useAppStore((s) => s.kickControlled);
  const kickViewed = useAppStore((s) => s.kickViewed);
  const stopMic = useAppStore((s) => s.stopMic);
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    // 挂载时和后端对账（应用重启时可能已有活动会话），随后靠事件 + 本地计时
    void refreshControl();
    void refreshViewed();
    void refreshMic();
  }, [refreshControl, refreshViewed, refreshMic]);

  useEffect(() => {
    if (controlled.length === 0 && viewed.length === 0 && micActive.length === 0) return;
    const timer = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(timer);
  }, [controlled.length, viewed.length, micActive.length]);

  if (controlled.length === 0 && viewed.length === 0 && micActive.length === 0) return null;
  return (
    <div className="ctrlbanner" data-control-banner>
      <Icon n="shield" s={14} />
      <div className="cb-main">
        {controlled.map((c) => {
          const secs = Math.max(0, Math.floor((now - (c.startedMs || now)) / 1000));
          return (
            <div key={c.sessionId} className="cb-row" data-controlled-id={c.sessionId}>
              <span className="cb-text">
                {T('ctrl.banner', { 0: c.peerName })}
                <span className="mono cb-sub"> {c.peerAddr} · {c.cols}×{c.rows} · {secs}s</span>
              </span>
              <button className="tbtn danger" data-kick onClick={() => void kickControl(c.sessionId)}>
                {T('ctrl.kick')}
              </button>
            </div>
          );
        })}
        {viewed.map((v) => {
          const secs = Math.max(0, Math.floor((now - (v.startedMs || now)) / 1000));
          return (
            <div key={v.sessionId} className="cb-row" data-viewed-id={v.sessionId}>
              <span className="cb-text">
                {T('rd.banner', { 0: v.peerName })}
                <span className="mono cb-sub"> {v.peerAddr} · {secs}s</span>
              </span>
              <button className="tbtn danger" data-screen-kick onClick={() => void kickViewed(v.sessionId)}>
                {T('ctrl.kick')}
              </button>
            </div>
          );
        })}
        {micActive.map((m) => (
          <div key={'mic-' + m.sessionId} className="cb-row" data-mic-id={m.sessionId}>
            <span className="cb-text">{T('mic.banner', { 0: m.peerName })}</span>
            <button className="tbtn danger" data-mic-stop onClick={() => void stopMic(m.sessionId)}>
              {T('mic.stop')}
            </button>
          </div>
        ))}
      </div>
    </div>
  );
}
