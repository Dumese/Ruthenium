// 小型表单 Sheet：片段与转发规则共用外壳
import { Sheet } from '../overlays/Sheet';
import { Button } from '../ui/Button';
import { Pill } from '../ui/Pill';
import { useAppStore } from '../../store/useAppStore';
import { SnippetForm } from '../snippets/SnippetForm';
import { RuleForm } from '../tunnels/RuleForm';
import { T, useLang } from '../../i18n';

export function SmallFormSheet() {
  useLang();
  const form = useAppStore((s) => s.form);
  const dispatch = useAppStore((s) => s.dispatch);
  if (!form) return null;
  const isSnip = form._t === 'snip';
  return (
    <Sheet
      title={form.id
        ? (isSnip ? T('sf.editSnippet') : T('sf.editRule'))
        : (isSnip ? T('sf.newSnippet') : T('sf.newRule'))}
      foot={
        <>
          <span className="sp" />
          <Button onClick={() => dispatch('closeov')}>{T('c.cancel')}</Button>
          <Button icon="check" primary onClick={() => dispatch('formsave')}>{T('c.save')}</Button>
        </>
      }
    >
      {isSnip ? <SnippetForm /> : <RuleForm />}
    </Sheet>
  );
}

/** 片段标签 pill 组（被 SnippetForm 引用，导出复用） */
export function TagPills({ value, onChange }: { value: string; onChange: (t: string) => void }) {
  useLang();
  const tags = [T('tag.diag'), T('tag.log'), T('tag.dev'), T('tag.maint'), T('tag.net')];
  return (
    <div className="pillrow">
      {tags.map((t) => (
        <Pill key={t} on={value === t} onClick={() => onChange(t)}>{t}</Pill>
      ))}
    </div>
  );
}
