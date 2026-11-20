// 终端内查找条：锚定在终端右上角的小面板，复用 palette 的视觉语言。
// 只在终端标签出现；输入即时 incremental 查找，↑/↓ 前后跳，Enter 下一个，Esc 关闭并清高亮。
// 计数来自 SearchAddon 的 onDidChangeResults（n/m），无匹配时显示 0/0 并给输入框红边。
import { useEffect, useRef, useState } from 'react';
import { Icon } from '../ui/Icon';
import { useAppStore } from '../../store/useAppStore';
import { clearTermSearch, onTermSearchResults, searchNext, searchPrevious } from './useXterm';
import { T, useLang } from '../../i18n';

export function TermFindBar({ sessionId }: { sessionId: string }) {
  useLang();
  const close = useAppStore((s) => s.closeTermFind);
  const [q, setQ] = useState('');
  const [hit, setHit] = useState<{ index: number; count: number }>({ index: -1, count: 0 });
  const inputRef = useRef<HTMLInputElement | null>(null);
  const bad = q.trim().length > 0 && hit.count === 0;

  useEffect(() => {
    const el = inputRef.current;
    el?.focus();
    el?.select();
    // 结果变化 → 更新 n/m；卸载（Esc / 切标签）时清装饰与选中
    const off = onTermSearchResults(sessionId, (r) => setHit({ index: r.resultIndex, count: r.resultCount }));
    return () => { off(); clearTermSearch(sessionId); };
  }, [sessionId]);

  const next = () => { if (q.trim()) searchNext(sessionId, q, false); };
  const prev = () => { if (q.trim()) searchPrevious(sessionId, q); };

  const onChange = (v: string) => {
    setQ(v);
    if (!v.trim()) {
      clearTermSearch(sessionId);
      setHit({ index: -1, count: 0 });
      return;
    }
    searchNext(sessionId, v, true); // 输入即时增量查找
  };

  const count = hit.count > 0
    ? `${(hit.index >= 0 ? hit.index : 0) + 1}/${hit.count}`
    : '0/0';

  return (
    <div className="termfind" role="search" aria-label={T('find.title')} data-find="bar">
      <Icon n="search" s={13} />
      <input
        ref={inputRef}
        className={'tfin' + (bad ? ' bad' : '')}
        data-find="input"
        value={q}
        placeholder={T('find.ph')}
        autoComplete="off"
        spellCheck={false}
        aria-invalid={bad || undefined}
        title={bad ? T('find.noMatch') : undefined}
        onChange={(e) => onChange(e.target.value)}
        onKeyDown={(e) => {
          // Esc 由 useHotkeys 的 capture 统一处理（先关查找条，再关 overlay）
          if (e.key === 'Enter') { e.preventDefault(); if (e.shiftKey) prev(); else next(); return; }
          if (e.key === 'ArrowUp') { e.preventDefault(); prev(); return; }
          if (e.key === 'ArrowDown') { e.preventDefault(); next(); }
        }}
      />
      <span className="tfcnt" data-find="count" data-empty={bad ? '1' : undefined}>{count}</span>
      <button type="button" className="tfbtn" data-find="prev" title={T('find.prev')} aria-label={T('find.prev')} onClick={prev}>↑</button>
      <button type="button" className="tfbtn" data-find="next" title={T('find.next')} aria-label={T('find.next')} onClick={next}>↓</button>
      <button type="button" className="tfbtn" data-find="close" title={T('find.close')} aria-label={T('find.close')} onClick={() => close()}>
        <Icon n="x" s={11} />
      </button>
    </div>
  );
}
