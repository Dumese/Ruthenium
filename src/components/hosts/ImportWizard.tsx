// 导入 %USERPROFILE%\.ssh\config 两步向导。前端预览解析，后端为权威。
import { useEffect, useState } from 'react';
import { Sheet } from '../overlays/Sheet';
import { Field } from '../ui/Field';
import { Button } from '../ui/Button';
import { Chip } from '../ui/Chip';
import { Icon } from '../ui/Icon';
import { useAppStore } from '../../store/useAppStore';
import { T, useLang } from '../../i18n';

// UI 引导示例（textarea 占位内容，不属于 mock 数据；Windows OpenSSH 路径写法）
const IMPORT_SAMPLE = 'Host web-prod-01\n  HostName 203.0.113.10\n  User ops\n  Port 22\n  IdentityFile %USERPROFILE%\\.ssh\\id_ed25519\n\nHost edge-cache\n  HostName 203.0.113.24\n  User ops\n  Port 2202\n\nHost legacy-box\n  HostName 198.51.100.7\n  User admin\n  Port 22\n';

export function ImportWizard() {
  useLang();
  const importParsed = useAppStore((s) => s.importParsed);
  const importSelected = useAppStore((s) => s.importSelected);
  const importText = useAppStore((s) => s.importText);
  const setImportText = useAppStore((s) => s.setImportText);
  const dispatch = useAppStore((s) => s.dispatch);
  const [text, setText] = useState(importText ?? IMPORT_SAMPLE);
  // 文本框初值必须同步进 store。store 的 parseImportText 用 `importText ?? IMPORT_SAMPLE`，
  // 而生产构建把 mock 替身的 IMPORT_SAMPLE 换成空串 —— 于是"看得见示例文本、点解析却报
  // 导入内容为空"。这里在挂载时（store 还没有文本）把默认内容写进去。
  useEffect(() => {
    if (useAppStore.getState().importText == null) setImportText(text);
  }, [text, setImportText]);
  const parsed = importParsed;
  const selectedCount = Object.values(importSelected).filter(Boolean).length;
  return (
    <Sheet
      title={T('imp.title')}
      foot={
        <>
          {parsed.length ? <span className="sidehint">{T('imp.pending', { 0: selectedCount })}</span> : null}
          <span className="sp" />
          <Button onClick={() => dispatch('closeov')}>{T('c.cancel')}</Button>
          {parsed.length
            ? <Button icon="check" primary onClick={() => dispatch('impdo')}>{T('imp.do')}</Button>
            : <Button icon="check" primary onClick={() => dispatch('impparse')}>{T('imp.parse')}</Button>}
        </>
      }
    >
      {!parsed.length ? (
        <Field label={T('imp.content')}>
          <textarea
            className="inp"
            id="impText"
            rows={13}
            value={text}
            onChange={(e) => { setText(e.target.value); setImportText(e.target.value); }}
          />
        </Field>
      ) : (
        <>
          {parsed.map((x, i) => {
            const on = !!importSelected[i];
            const dup = x.duplicate;
            return (
              <button key={i} className={'checkrow' + (on ? ' on' : '')} onClick={() => dispatch('imptoggle', String(i))}>
                <span className="cb">{on && <Icon n="check" s={11} />}</span>
                <span className="cr"><b>{x.name}</b> <span className="cs">{x.user}@{x.addr}:{x.port}</span></span>
                {dup ? <Chip>{T('imp.exists')}</Chip> : <Chip>{T('imp.new')}</Chip>}
              </button>
            );
          })}
          <div style={{ marginTop: 10 }}>
            <Button icon="arrow" onClick={() => dispatch('impback')}>{T('imp.back')}</Button>
          </div>
        </>
      )}
    </Sheet>
  );
}
