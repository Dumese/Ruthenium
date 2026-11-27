// 对端请求采集本机麦克风：每次开启都要本机确认，30 秒不操作自动拒绝。
// 确认成功后由采集线程回传实际状态（打开失败/被本机停止会再提示）。
import { Modal } from '../overlays/Sheet';
import { useAppStore } from '../../store/useAppStore';
import { T, useLang } from '../../i18n';
import { useConsentCountdown } from './useConsentCountdown';

export function PeerMicDialog() {
  useLang();
  const req = useAppStore((s) => s.micReq);
  const busy = useAppStore((s) => s.micBusy);
  const consent = useAppStore((s) => s.micConsent);
  const left = useConsentCountdown(req?.consentId, 30, () => void consent(false));

  if (!req) return null;
  return (
    <Modal onClose={busy ? () => { /* 决策在途 */ } : () => void consent(false)}>
      <div className="modalbody">
        <h3>{T('mic.requestTitle')}</h3>
        <p>{T('mic.requestBody', { 0: req.peerName })}</p>
        <div className="ctrl-meta mono">{req.sessionId}</div>
        <p className="pair-hint">{T('mic.requestHint')}</p>
        <p className="consent-countdown" data-consent-left={left}>
          {T('ctrl.autoReject', { 0: String(left) })}
        </p>
      </div>
      <div className="modalfoot">
        <button className="muted" data-mic-reject disabled={busy} onClick={() => void consent(false)}>
          {T('ctrl.reject')}
        </button>
        <button className="strong" data-mic-allow disabled={busy} onClick={() => void consent(true)}>
          {T('mic.allow')}
        </button>
      </div>
    </Modal>
  );
}
