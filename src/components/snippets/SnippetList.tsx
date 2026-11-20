// 侧栏片段列表：点行插入当前终端输入行，不执行
import { Icon } from '../ui/Icon';
import { useAppStore } from '../../store/useAppStore';
import { T, useLang } from '../../i18n';

export function SnippetList() {
  useLang();
  const snippets = useAppStore((s) => s.snippets);
  const search = useAppStore((s) => s.search);
  const dispatch = useAppStore((s) => s.dispatch);
  const q = search.toLowerCase();
  const list = snippets.filter((x) => !q || (x.name + x.cmd + x.tag).toLowerCase().includes(q));
  return (
    <>
      <div className="slabel">{T('side.snipLabel')}<span className="sp" /><button onClick={() => dispatch('snipnew')}>{T('side.new')}</button></div>
      {!list.length && <div className="emptyhint">{T('side.noSnip')}</div>}
      {list.map((n) => (
        <button key={n.id} className="listitem" title={T('side.snipTitle')}
          onClick={() => dispatch('snipinsert', n.id)}>
          <Icon n="code" s={14} />
          <span className="li">
            <span className="lt">{n.name}</span>
            <span className="ls mono">{n.cmd}</span>
          </span>
          <Icon n="arrow" s={12} className="chev" />
        </button>
      ))}
      <div className="emptyhint emptytip">
        {T('side.snipTip')}
      </div>
    </>
  );
}
