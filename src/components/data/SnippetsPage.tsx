// 片段数据页
import { Button } from '../ui/Button';
import { useAppStore } from '../../store/useAppStore';
import { SnippetCard } from '../snippets/SnippetCard';
import { T, useLang } from '../../i18n';

export function SnippetsPage() {
  useLang();
  const snippets = useAppStore((s) => s.snippets);
  const dispatch = useAppStore((s) => s.dispatch);
  return (
    <div className="datapage">
      <div className="dphead">
        <div>
          <h2>{T('snips.title')}</h2>
          <p>{T('snips.sub')}</p>
        </div>
        <span className="sp" />
        <Button icon="plus" primary onClick={() => dispatch('snipnew')}>{T('snips.new')}</Button>
      </div>
      <div className="dpgrid">
        {snippets.map((n) => <SnippetCard key={n.id} snip={n} />)}
      </div>
    </div>
  );
}
