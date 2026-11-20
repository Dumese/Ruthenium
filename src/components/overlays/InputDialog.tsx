// 输入弹窗：新建文件夹 / 重命名
import { useEffect, useRef } from 'react';
import { Modal, ModalIcon } from './Sheet';
import { useAppStore } from '../../store/useAppStore';
import { T, useLang } from '../../i18n';

export function InputDialog() {
  useLang();
  const input = useAppStore((s) => s.input);
  const dispatch = useAppStore((s) => s.dispatch);
  const ref = useRef<HTMLInputElement | null>(null);
  const rename = input?.mode === 'rename';
  const group = input?.mode === 'newgroup';
  const groupRename = input?.mode === 'grouprename';
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.focus();
    el.setSelectionRange(el.value.length, el.value.length);
  }, [input?.mode]);
  if (!input) return null;
  return (
    <Modal>
      <div className="modalbody">
        <ModalIcon icon={group || groupRename ? 'layers' : rename ? 'edit' : 'folder'} />
        <h3>{input.title}</h3>
        <p>{input.text}</p>
        <input
          ref={ref}
          className="inp"
          id={group ? 'groupName' : rename ? 'renameName' : 'mkdirName'}
          style={{ marginTop: 14, textAlign: 'left' }}
          placeholder={T(group ? 'grp.ph' : rename ? 'rn.ph' : 'mk.ph')}
          autoComplete="off"
          spellCheck={false}
          value={input.value}
          onChange={(e) => useAppStore.setState({ input: { ...input, value: e.target.value } })}
          onKeyDown={(e) => {
            if (e.key !== 'Enter') return;
            e.preventDefault();
            dispatch(group ? 'groupsave' : groupRename ? 'grouprenamesave' : rename ? 'renamesave' : 'mkdirsave');
          }}
        />
      </div>
      <div className="modalfoot">
        <button className="muted" onClick={() => dispatch('closeov')}>{T('c.cancel')}</button>
        <button className="strong" onClick={() => dispatch(group ? 'groupsave' : groupRename ? 'grouprenamesave' : rename ? 'renamesave' : 'mkdirsave')}>
          {T(group ? 'grp.create' : rename ? 'rn.save' : 'mk.create')}
        </button>
      </div>
    </Modal>
  );
}
