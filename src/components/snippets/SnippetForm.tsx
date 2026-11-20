// 片段表单
import { Field } from '../ui/Field';
import { useAppStore } from '../../store/useAppStore';
import { TagPills } from '../overlays/SmallFormSheet';
import { T, useLang } from '../../i18n';

export function SnippetForm() {
  useLang();
  const form = useAppStore((s) => s.form);
  const update = useAppStore((s) => s.updateForm);
  if (!form || form._t !== 'snip') return null;
  return (
    <>
      <Field label={T('sf.name')}>
        <input className="inp" placeholder={T('snip.n1')} value={form.name ?? ''} onChange={(e) => update('name', e.target.value)} />
      </Field>
      <Field label={T('sf.cmd')}>
        <input className="inp" placeholder="uptime && df -h /" value={form.cmd ?? ''} onChange={(e) => update('cmd', e.target.value)} />
      </Field>
      <Field label={T('hf.tag')}>
        <TagPills value={form.tag ?? T('tag.diag')} onChange={(t) => update('tag', t)} />
      </Field>
      <div className="notebox">{T('sf.snipNote')}</div>
    </>
  );
}
