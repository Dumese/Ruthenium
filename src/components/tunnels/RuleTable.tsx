// 转发规则表格；表单见 tunnels/RuleForm
import { Icon } from '../ui/Icon';
import { Dot } from '../ui/Dot';
import { useAppStore } from '../../store/useAppStore';
import { hostName } from '../../store/selectors';
import { T, useLang } from '../../i18n';

export function RuleTable() {
  useLang();
  const tunnels = useAppStore((s) => s.tunnels);
  const tunnelState = useAppStore((s) => s.tunnelState);
  const dispatch = useAppStore((s) => s.dispatch);
  const askConfirm = useAppStore((s) => s.askConfirm);
  /** 状态列优先反映**运行态**（tunnel://state），否则显示配置意图。 */
  const statusOf = (id: string, on: boolean): { dot: 'on' | 'off' | 'warn'; label: string; title?: string } => {
    const rt = tunnelState[id];
    if (rt?.error) return { dot: 'warn', label: T('rules.error'), title: rt.error };
    if (rt?.active) return { dot: 'on', label: T('rules.listening') };
    if (on) return { dot: 'off', label: T('rules.pending') };
    return { dot: 'off', label: T('rules.inactive') };
  };
  return (
    <table className="dptable">
      <thead>
        <tr>
          <th>{T('th.name')}</th><th>{T('th.type')}</th><th>{T('th.localAddr')}</th><th>{T('th.target')}</th><th>{T('th.hostLink')}</th><th>{T('th.status')}</th>
          <th style={{ textAlign: 'right' }}>{T('th.actions')}</th>
        </tr>
      </thead>
      <tbody>
        {tunnels.map((r) => {
          const st = statusOf(r.id, r.on);
          return (
            <tr key={r.id}>
              <td className="strong">{r.name}</td>
              <td>{r.kind === 'local' ? T('sf.local') : T('sf.remote')}</td>
              <td className="mono">{r.local}</td>
              <td className="mono">{r.remote}</td>
              <td>{hostName(r.host)}</td>
              <td title={st.title ?? ''}><Dot state={st.dot} /> {st.label}</td>
              <td>
                <div className="acts">
                  <button title={T('rules.toggle')} onClick={() => dispatch('ruletoggle', r.id)}><Icon n={r.on ? 'pause' : 'play'} s={13} /></button>
                  <button title={T('c.edit')} onClick={() => dispatch('ruleedit', r.id)}><Icon n="edit" s={13} /></button>
                  <button title={T('c.delete')} onClick={() => askConfirm('ruledel', r.id, T('c.delete'), T('toast.delFileWarn', { 0: r.name }))}><Icon n="trash" s={13} /></button>
                </div>
              </td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}
