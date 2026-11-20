// 主机密钥指纹确认 Modal（真实流程由 hostkey://challenge 事件驱动）
import { Modal, ModalIcon } from './Sheet';
import { useAppStore } from '../../store/useAppStore';
import { T, useLang } from '../../i18n';

export function HostKeyModal() {
  useLang();
  const hk = useAppStore((s) => s.hostkey);
  const dispatch = useAppStore((s) => s.dispatch);
  if (!hk) return null;
  return (
    <Modal>
      <div className="modalbody">
        <ModalIcon icon="key" danger={hk.changed} />
        <h3>{T('hk.title', { 0: hk.host })}</h3>
        <p>{T('hk.body')}</p>
        <div className="fingerprint mono">
          {hk.host} ({hk.port})<br />
          {hk.algo} {hk.fingerprint}
          {hk.changed && <><br /><span style={{ color: 'var(--red)' }}>{T('err.E_SSH_HOSTKEY_CHANGED')}</span></>}
        </div>
      </div>
      <div className="modalfoot">
        <button className="muted" onClick={() => dispatch('hostkeyreject')}>{T('hk.reject')}</button>
        <button className="strong" onClick={() => dispatch('hostkeyaccept')}>{T('hk.accept')}</button>
      </div>
    </Modal>
  );
}
