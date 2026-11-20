// 连接时的密码弹窗。
// 底部两个动作按钮：仅本次连接 / 保存并连接（Enter 默认保存并连接）；Esc / 点击外部取消。
// 密码明文只存在这个组件的本地 state 里：提交 / 取消 / 卸载即丢弃，绝不进 store 或 host。
import { useEffect, useRef, useState } from 'react';
import { Modal, ModalIcon } from './Sheet';
import { Icon } from '../ui/Icon';
import { useAppStore } from '../../store/useAppStore';
import { T, useLang } from '../../i18n';

export function PasswordModal() {
  useLang();
  const pw = useAppStore((s) => s.pw);
  const hosts = useAppStore((s) => s.hosts);
  const submit = useAppStore((s) => s.submitPassword);
  const closeOverlay = useAppStore((s) => s.closeOverlay);
  const [value, setValue] = useState('');
  const ref = useRef<HTMLInputElement | null>(null);

  useEffect(() => {
    // 弹窗复用（同一 overlay 换目标主机）时绝不能把上一台的密码提交给下一台。
    setValue('');
    ref.current?.focus();
  }, [pw?.hostId, pw?.retry, pw?.reconnectId]);

  const host = pw ? hosts.find((h) => h.id === pw.hostId) : undefined;
  if (!pw || !host) return null;

  /** save=true：先 set_password 成功再 connect；false：密码只随本次 connect 下行 */
  const doSubmit = (save: boolean) => {
    const v = value;
    setValue('');                                       // 立刻从组件内存里抹掉
    void submit(v, save);
  };

  return (
    <Modal>
      <div className="modalbody">
        <ModalIcon icon="key" />
        <h3>{T('pw.title')}</h3>
        <p>{T('pw.body', { 0: host.name })}</p>
        {pw.retry && <p className="pwretry">{T('pw.retryHint')}</p>}
        <input
          ref={ref}
          id="pwInput"
          className="inp"
          type="password"
          style={{ marginTop: 14, textAlign: 'left' }}
          placeholder={T('pw.ph')}
          autoComplete="off"
          spellCheck={false}
          value={value}
          onChange={(e) => setValue(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter') { e.preventDefault(); doSubmit(true); }   // Enter 默认「保存并连接」
            else if (e.key === 'Escape') { e.preventDefault(); closeOverlay(); }   // 取消不建会话
          }}
        />
        <p className="pwtip">{T('pw.saveTip')}</p>
      </div>
      {/* 两个动作按钮；取消走 Esc / 点遮罩（不建会话、不保存），不再单列按钮 */}
      <div className="modalfoot">
        <button data-act="pwsubmit" onClick={() => doSubmit(false)}>{T('pw.connectOnly')}</button>
        <button className="strong" data-act="pwsaveconnect" title={T('pw.saveTip')} onClick={() => doSubmit(true)}>
          <Icon n="key" s={13} />{T('pw.saveConnect')}
        </button>
      </div>
    </Modal>
  );
}
