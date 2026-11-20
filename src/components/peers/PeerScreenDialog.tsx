// 对端请求查看本机屏幕：每次都要本机确认，30 秒不操作自动拒绝。
// 默认「只看」；勾选「允许操作」后才会开启键鼠输入通道（另一端仍需逐步操作）。
// 开启启动验证时，确认后还会过一次 Windows Hello（后端执行）。
import { useEffect, useState } from 'react';
import { useAppStore } from '../../store/useAppStore';
import { Modal } from '../overlays/Sheet';
import { T, useLang } from '../../i18n';
import { useConsentCountdown } from './useConsentCountdown';

export function PeerScreenDialog() {
  useLang();
  const req = useAppStore((s) => s.screenReq);
  const busy = useAppStore((s) => s.screenBusy);
  const consent = useAppStore((s) => s.screenConsent);
  const [allowInput, setAllowInput] = useState(false);
  const left = useConsentCountdown(req?.consentId, 30, () => void consent(false));

  // 每次新请求重置为「只看」
  useEffect(() => {
    setAllowInput(false);
  }, [req?.consentId]);

  if (!req) return null;
  const inputOnly = req.inputOnly === true;
  return (
    <Modal onClose={busy ? () => { /* 决策在途 */ } : () => void consent(false, false)}>
      <div className="modalbody">
        <h3>{T(inputOnly ? 'rd.inputRequestTitle' : 'rd.requestTitle')}</h3>
        <p>
          {inputOnly
            ? T('rd.inputRequestBody', { 0: req.peerName, 1: req.peerFp })
            : T('rd.requestBody', { 0: req.peerName, 1: req.peerFp })}
        </p>
        <div className="ctrl-meta mono">{req.addr}</div>
        {req.inputRequested && !inputOnly && (
          <div className="screen-allow-input">
            <input
              id="screen-allow-input"
              type="checkbox"
              data-screen-allow-input
              checked={allowInput}
              onChange={(e) => setAllowInput(e.currentTarget.checked)}
              data-allow-state={allowInput ? '1' : '0'}
            />
            <label htmlFor="screen-allow-input">{T('rd.allowInput')}</label>
          </div>
        )}
        <p className="pair-hint">
          {req.helloRequired ? T('ctrl.helloNote') : T('rd.requestHint')}
        </p>
        <p className="consent-countdown" data-consent-left={left}>
          {T('ctrl.autoReject', { 0: String(left) })}
        </p>
      </div>
      <div className="modalfoot">
        <button className="muted" data-screen-reject disabled={busy} onClick={() => void consent(false)}>
          {T('ctrl.reject')}
        </button>
        <button className="strong" data-screen-allow disabled={busy} onClick={() => void consent(true, allowInput)}>
          {T(inputOnly || allowInput ? 'rd.allowControl' : 'rd.allow')}
        </button>
      </div>
    </Modal>
  );
}
