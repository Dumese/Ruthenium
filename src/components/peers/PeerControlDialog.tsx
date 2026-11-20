// 对端请求控制本机：模态确认（每次都要点，30 秒不操作自动拒绝）。
// 开启「启动时使用 Windows Hello」时，确认后还会再过一次系统验证（后端执行）。
import { useAppStore } from '../../store/useAppStore';
import { Modal } from '../overlays/Sheet';
import { T, useLang } from '../../i18n';
import { useConsentCountdown } from './useConsentCountdown';

export function PeerControlDialog() {
  useLang();
  const req = useAppStore((s) => s.controlReq);
  const busy = useAppStore((s) => s.controlBusy);
  const consentControl = useAppStore((s) => s.consentControl);
  const left = useConsentCountdown(req?.consentId, 30, () => void consentControl(false));
  if (!req) return null;
  return (
    <Modal onClose={busy ? () => { /* 决策在途 */ } : () => void consentControl(false)}>
      <div className="modalbody">
        <h3>{T('ctrl.requestTitle')}</h3>
        <p>{T('ctrl.requestBody', { 0: req.peerName, 1: req.peerFp })}</p>
        <div className="ctrl-meta mono">
          {req.addr} · {req.cols}×{req.rows}
        </div>
        <p className="pair-hint">
          {req.helloRequired ? T('ctrl.helloNote') : T('ctrl.requestHint')}
        </p>
        <p className="consent-countdown" data-consent-left={left}>
          {T('ctrl.autoReject', { 0: String(left) })}
        </p>
      </div>
      <div className="modalfoot">
        <button
          className="muted"
          data-control-reject
          disabled={busy}
          onClick={() => void consentControl(false)}
        >
          {T('ctrl.reject')}
        </button>
        <button
          className="strong danger-text"
          data-control-allow
          disabled={busy}
          onClick={() => void consentControl(true)}
        >
          {T('ctrl.allow')}
        </button>
      </div>
    </Modal>
  );
}
