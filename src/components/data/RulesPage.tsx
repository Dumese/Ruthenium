// 转发规则数据页
import { Button } from '../ui/Button';
import { useAppStore } from '../../store/useAppStore';
import { RuleTable } from '../tunnels/RuleTable';
import { T, useLang } from '../../i18n';

export function RulesPage() {
  useLang();
  const dispatch = useAppStore((s) => s.dispatch);
  return (
    <div className="datapage">
      <div className="dphead">
        <div>
          <h2>{T('rules.title')}</h2>
          <p>{T('rules.sub')}</p>
        </div>
        <span className="sp" />
        <Button icon="plus" primary onClick={() => dispatch('rulenew')}>{T('rules.new')}</Button>
      </div>
      <RuleTable />
    </div>
  );
}
