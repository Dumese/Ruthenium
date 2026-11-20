// 确认弹窗
import { Modal, ModalIcon } from './Sheet';
import { useAppStore } from '../../store/useAppStore';
import { T, useLang } from '../../i18n';

export function ConfirmDialog() {
  useLang();
  const c = useAppStore((s) => s.confirm);
  const dispatch = useAppStore((s) => s.dispatch);
  return (
    <Modal>
      <div className="modalbody">
        <ModalIcon icon="alert" danger />
        <h3>{c?.title || T('cf.title')}</h3>
        <p>{c?.text || T('cf.body')}</p>
      </div>
      <div className="modalfoot">
        <button className="muted" onClick={() => dispatch('closeov')}>{T('c.cancel')}</button>
        <button className="strong" style={{ color: 'var(--red)' }} onClick={() => dispatch('confirmyes')}>{T('cf.yes')}</button>
      </div>
    </Modal>
  );
}
