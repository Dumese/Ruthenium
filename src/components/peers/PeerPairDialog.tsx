// 配对弹窗：严格/手动地址显示 6 位码两端核对；一键模式只确认设备与授权。
// 被请求方额外提供六项授权勾选（默认沿用上次选择，首次全不勾）。
// 结果由 `peer://pairing` 事件驱动。
import { useEffect, useRef, useState } from 'react';
import { useAppStore } from '../../store/useAppStore';
import { Modal } from '../overlays/Sheet';
import { T, useLang } from '../../i18n';
import type { PeerGrants } from '../../ipc/commands';

/** 按对端指纹记住上次勾选的授权（只存布尔值，不含任何凭据）。 */
function grantsKey(fp: string): string {
  return `ruthenium.pair.grants.${fp}`;
}

function loadGrants(fp: string): Partial<PeerGrants> {
  if (!fp) return {};
  try {
    const raw = localStorage.getItem(grantsKey(fp));
    return raw ? (JSON.parse(raw) as Partial<PeerGrants>) : {};
  } catch {
    return {};
  }
}

function saveGrants(fp: string, g: PeerGrants): void {
  if (!fp) return;
  try {
    localStorage.setItem(grantsKey(fp), JSON.stringify(g));
  } catch {
    /* 隐私模式/配额满：不影响配对 */
  }
}

export function PeerPairDialog() {
  useLang();
  const pair = useAppStore((s) => s.pair);
  const pairBusy = useAppStore((s) => s.pairBusy);
  const pairConfirm = useAppStore((s) => s.pairConfirm);
  const pairCancel = useAppStore((s) => s.pairCancel);
  const [files, setFiles] = useState(false);
  const [control, setControl] = useState(false);
  const [config, setConfig] = useState(false);
  const [screen, setScreen] = useState(false);
  const [clipboard, setClipboard] = useState(false);
  const [audio, setAudio] = useState(false);
  const [left, setLeft] = useState(0);
  const cancelFired = useRef(false);

  // 新弹窗：按对端指纹恢复上次授权选择（默认全不勾）
  useEffect(() => {
    const g = loadGrants(pair?.peerFp ?? '');
    setFiles(!!g.files);
    setControl(!!g.control);
    setConfig(!!g.config);
    setScreen(!!g.screen);
    setClipboard(!!g.clipboard);
    setAudio(!!g.audio);
  }, [pair?.pairingId, pair?.peerFp]);

  // 后端窗口截止时间（Unix ms）→ 倒计时；到点自动取消，避免展示已失效请求。
  useEffect(() => {
    cancelFired.current = false;
    if (!pair) { setLeft(0); return; }
    // 已点「允许/拒绝」：决策已送达，后续由后端终止事件收尾，不再按确认窗口倒计时。
    if (pairBusy) { setLeft(0); return; }
    const deadline = Number(pair.expiresAtMs);
    // 旧后端/异常事件没有截止时间：不显示倒计时，也不自动取消。
    if (!Number.isFinite(deadline) || deadline <= 0) { setLeft(0); return; }
    const tick = () => Math.max(0, Math.ceil((deadline - Date.now()) / 1000));
    setLeft(tick());
    const timer = setInterval(() => {
      const n = tick();
      setLeft(n);
      if (n <= 0 && !cancelFired.current) {
        cancelFired.current = true;
        pairCancel();
      }
    }, 1000);
    return () => clearInterval(timer);
  }, [pair?.pairingId, pair?.expiresAtMs, pair, pairBusy, pairCancel]);

  if (!pair) return null;
  const incoming = pair.direction === 'incoming';
  // 旧后端/异常事件没有 numeric 字段时按严格处理，避免静默隐藏 6 位码。
  const numeric = pair.numeric !== false;
  const grants: PeerGrants = { files, control, config, screen, clipboard, audio };
  const agree = () => {
    // 只记住用户本次的选择；不记录任何设备凭据。
    saveGrants(pair.peerFp, grants);
    void pairConfirm(true, grants);
  };
  return (
    <Modal onClose={pairBusy ? () => { /* 决策在途：等终止事件 */ } : pairCancel}>
      <div className="modalbody">
        <h3>{T('pair.title')}</h3>
        <p>{incoming ? T('pair.incoming', { 0: pair.peerName }) : T('pair.outgoing', { 0: pair.peerName })}</p>
        <div className="pair-addr mono">{pair.peerAddr}</div>
        {numeric ? (
          <>
            <div className="pair-code" data-pair-code>{pair.code}</div>
            <p className="pair-hint">{T('pair.hint')}</p>
          </>
        ) : (
          <p className="pair-hint">{T('pair.justWorksHint')}</p>
        )}
        <div className="pair-fp mono">{pair.peerFp}</div>
        {incoming && (
          <div className="pair-grants">
            <label>
              <input type="checkbox" data-grant="files" checked={files} onChange={(e) => setFiles(e.target.checked)} />
              {T('pair.grantFiles')}
            </label>
            <label className="pair-warn">
              <input type="checkbox" data-grant="control" checked={control} onChange={(e) => setControl(e.target.checked)} />
              {T('pair.grantControl')}
            </label>
            <label>
              <input type="checkbox" data-grant="config" checked={config} onChange={(e) => setConfig(e.target.checked)} />
              {T('pair.grantConfig')}
            </label>
            <label className="pair-warn">
              <input type="checkbox" data-grant="screen" checked={screen} onChange={(e) => setScreen(e.target.checked)} />
              {T('pair.grantScreen')}
            </label>
            <label className="pair-warn">
              <input type="checkbox" data-grant="clipboard" checked={clipboard} onChange={(e) => setClipboard(e.target.checked)} />
              {T('pair.grantClipboard')}
            </label>
            <label className="pair-warn">
              <input type="checkbox" data-grant="audio" checked={audio} onChange={(e) => setAudio(e.target.checked)} />
              {T('pair.grantAudio')}
            </label>
          </div>
        )}
        {!pairBusy && pair.expiresAtMs > 0 && (
          <p className="consent-countdown" data-pair-left={left}>
            {T('pair.expires', { 0: String(left) })}
          </p>
        )}
      </div>
      <div className="modalfoot">
        <button
          className="muted"
          data-pair-disagree
          disabled={pairBusy}
          onClick={() => void pairConfirm(false, { files: false, control: false, config: false, screen: false, clipboard: false, audio: false })}
        >
          {numeric ? T('pair.disagree') : T('pair.deny')}
        </button>
        <button
          className="strong"
          data-pair-agree
          data-pair-numeric={numeric}
          disabled={pairBusy}
          onClick={agree}
        >
          {numeric ? T('pair.agree') : T('pair.allow')}
        </button>
      </div>
    </Modal>
  );
}
