// 侧栏转发规则列表（rules 分支）：点行启停
import { Icon } from '../ui/Icon';
import { useAppStore } from '../../store/useAppStore';
import { T, useLang } from '../../i18n';

export function RuleList() {
  useLang();
  const tunnels = useAppStore((s) => s.tunnels);
  const tunnelState = useAppStore((s) => s.tunnelState);
  const search = useAppStore((s) => s.search);
  const dispatch = useAppStore((s) => s.dispatch);
  const q = search.toLowerCase();
  const list = tunnels.filter((x) => !q || (x.name + x.local + x.remote).toLowerCase().includes(q));
  return (
    <>
      <div className="slabel">{T('side.ruleLabel')}<span className="sp" /><button onClick={() => dispatch('rulenew')}>{T('side.new')}</button></div>
      {!list.length && <div className="emptyhint">{T('side.noRule')}</div>}
      {list.map((r) => {
        // 有运行态就显示真实监听状态，没有则退回配置意图 on。
        const rt = tunnelState[r.id];
        const onDot = rt ? rt.active : r.on;
        return (
          <button key={r.id} className="listitem" title={rt?.error ?? T('side.ruleTitle')}
            onClick={() => dispatch('ruletoggle', r.id)}>
            <Icon n="network" s={14} />
            <span className="li">
              <span className="lt">{r.name}</span>
              <span className="ls mono">{r.local} → {r.remote}</span>
            </span>
            <span className={'dot ' + (rt?.error ? 'warn' : onDot ? 'on' : 'off')} style={{ marginRight: 5 }} />
          </button>
        );
      })}
      <div className="emptyhint emptytip">
        {T('side.ruleTip')}
      </div>
    </>
  );
}
