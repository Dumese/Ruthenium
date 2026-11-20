// 转发规则表单。只允许 kind=local 的真实转发，UI 保留 remote 选项与模型一致。
import { Field } from '../ui/Field';
import { useAppStore } from '../../store/useAppStore';
import { T, useLang } from '../../i18n';

/** 本地绑定地址是否回环（127.x / localhost / [::1]）。非回环绑定会把
 *  远端服务暴露到局域网——必须给用户显式警告。 */
function isLoopbackBind(local: string): boolean {
  const host = local.split(':')[0]?.toLowerCase() ?? '';
  return host === '127.0.0.1' || host === 'localhost' || host === '[::1]' || host === '::1' || host === '';
}

export function RuleForm() {
  useLang();
  const form = useAppStore((s) => s.form);
  const hosts = useAppStore((s) => s.hosts);
  const update = useAppStore((s) => s.updateForm);
  if (!form || form._t !== 'rule') return null;
  const nonLoopback = !isLoopbackBind(form.local ?? '');
  return (
    <>
      <Field label={T('sf.name')}>
        <input className="inp" placeholder={T('rule.r1')} value={form.name ?? ''} onChange={(e) => update('name', e.target.value)} />
      </Field>
      <Field label={T('sf.localAddr')}>
        <input className="inp" placeholder="127.0.0.1:5432" value={form.local ?? ''} onChange={(e) => update('local', e.target.value)} />
      </Field>
      {nonLoopback && (
        <p className="fieldwarn">{T('rule.nonLoopback')}</p>
      )}
      <Field label={T('sf.target')}>
        <input className="inp" placeholder="10.20.0.11:5432" value={form.remote ?? ''} onChange={(e) => update('remote', e.target.value)} />
      </Field>
      <div className="grid2">
        <Field label={T('sf.type')}>
          {/* 只实现 -L 本地转发：不给 remote 选项，避免用户选出「保存必失败」的规则 */}
          <select className="inp" value="local" disabled>
            <option value="local">{T('sf.local')}</option>
          </select>
        </Field>
        <Field label={T('sf.host')}>
          <select className="inp" value={form.host ?? hosts[0]?.id ?? ''} onChange={(e) => update('host', e.target.value)}>
            {/* data-user：主机名是用户数据（i18n 残检据此跳过） */}
            {hosts.map((x) => <option key={x.id} value={x.id} data-user="1">{x.name}</option>)}
          </select>
        </Field>
      </div>
    </>
  );
}
