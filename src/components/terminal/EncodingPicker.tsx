// 终端编码选择：
// 状态栏右下角锚定面板（right:10px / bottom:34px），13 项固定列表、搜索、↑↓↵Esc、点外关闭。
// 编码是会话级状态（Session.encoding），切换只影响此后接收的字节。
import { useEffect, useMemo, useRef, useState } from 'react';
import { Icon } from '../ui/Icon';
import { useAppStore } from '../../store/useAppStore';
import { T, useLang } from '../../i18n';

export interface EncItem { enc: string; descKey: string; group: 'unicode'|'chinese'|'japanese'|'korean'|'other' }
// 顺序即默认推荐顺序（GBK 是 GB18030 的子集，GB18030 放前面）
const ENCODINGS: EncItem[] = [
  { enc: 'UTF-8', descKey: '', group: 'unicode' },
  { enc: 'UTF-8-BOM', descKey: '', group: 'unicode' },
  { enc: 'UTF-16LE', descKey: '', group: 'unicode' },
  { enc: 'UTF-16BE', descKey: '', group: 'unicode' },
  { enc: 'GB18030', descKey: 'enc.hans', group: 'chinese' },
  { enc: 'GBK', descKey: 'enc.hans', group: 'chinese' },
  { enc: 'Big5', descKey: 'enc.hant', group: 'chinese' },
  { enc: 'Shift_JIS', descKey: 'enc.ja', group: 'japanese' },
  { enc: 'EUC-JP', descKey: 'enc.ja', group: 'japanese' },
  { enc: 'EUC-KR', descKey: 'enc.ko', group: 'korean' },
  { enc: 'ISO-8859-1', descKey: 'enc.western', group: 'other' },
  { enc: 'Windows-1252', descKey: 'enc.western', group: 'other' },
  { enc: 'KOI8-R', descKey: 'enc.cyrillic', group: 'other' },
];
const GROUP_ORDER: EncItem['group'][] = ['unicode', 'chinese', 'japanese', 'korean', 'other'];

export function EncodingPicker() {
  const lang = useLang();
  const encOpen = useAppStore((s) => s.encOpen);
  const sessions = useAppStore((s) => s.sessions);
  const active = useAppStore((s) => s.active);
  const dispatch = useAppStore((s) => s.dispatch);
  const setEncOpen = useAppStore((s) => s.setEncOpen);
  const [q, setQ] = useState('');
  const [sel, setSel] = useState(0);
  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);

  const s = sessions.find((x) => x.id === active) ?? null;

  const filtered = useMemo(() => {
    const kw = q.trim().toLowerCase();
    return ENCODINGS.filter((x) => !kw || x.enc.toLowerCase().includes(kw) || (x.descKey && T(x.descKey).toLowerCase().includes(kw)));
  }, [q, lang]);

  // 打开时聚焦并重置；键盘 ↑↓↵Esc
  useEffect(() => {
    if (!encOpen) return;
    setQ(''); setSel(0);
    setTimeout(() => inputRef.current?.focus(), 0);
  }, [encOpen]);

  useEffect(() => {
    if (!encOpen) return;
    const onKey = (e: KeyboardEvent) => {
      // 面板打开时可能又有别的 overlay 弹出（Ctrl+K 命令面板等）。
      // 只有焦点仍在编码面板内（或尚未聚焦任何输入）时才接管 ↑↓/Enter/Esc，
      // 否则在命令面板里按 Enter 会“顺手”切换会话编码（中文立即变乱码）。
      const ae = document.activeElement as HTMLElement | null;
      const focusInside = !!ae && (ae.closest('.encpop') !== null || ae === document.body);
      if (!focusInside) return;
      if (e.key === 'ArrowDown') { e.preventDefault(); setSel((v) => Math.min(filtered.length - 1, v + 1)); }
      else if (e.key === 'ArrowUp') { e.preventDefault(); setSel((v) => Math.max(0, v - 1)); }
      else if (e.key === 'Enter') {
        e.preventDefault();
        const item = filtered[sel];
        if (item) dispatch('encset', item.enc);
      } else if (e.key === 'Escape') { e.preventDefault(); setEncOpen(false); }
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  }, [encOpen, filtered, sel, dispatch, setEncOpen]);

  // 高亮跟随键盘
  useEffect(() => {
    const el = listRef.current?.querySelector('.encrow.kb');
    el?.scrollIntoView({ block: 'nearest' });
  }, [sel, filtered]);

  if (!encOpen || !s) return null;

  return (
    <>
      {/* 透明 scrim：点击面板外关闭，不遮状态栏操作 */}
      <div className="scrim clear" onClick={() => setEncOpen(false)} />
      <div className="encpop" role="listbox">
        <div className="eh">
          {T('enc.title')}
          <span className="hchip">{s.title}</span>
        </div>
        <div className="encsearch">
          <Icon n="search" s={13} />
          <input
            ref={inputRef}
            value={q}
            placeholder={T('enc.search')}
            onChange={(e) => { setQ(e.target.value); setSel(0); }}
            spellCheck={false}
          />
        </div>
        <div className="enclist" ref={listRef}>
          {filtered.length === 0 && <div className="emptyhint enc-nomatch">{T('pal.noMatch')}</div>}
          {GROUP_ORDER.map((g) => {
            const items = filtered.filter((x) => x.group === g);
            if (!items.length) return null;
            return (
              <div key={g}>
                <div className="encgroup">{T('enc.group' + g[0].toUpperCase() + g.slice(1))}</div>
                {items.map((item) => {
                  const idx = filtered.indexOf(item);
                  const on = item.enc === s.encoding;
                  return (
                    <button
                      key={item.enc}
                      className={'encrow' + (on ? ' on' : '') + (idx === sel ? ' kb' : '')}
                      onClick={() => dispatch('encset', item.enc)}
                      onMouseEnter={() => setSel(idx)}
                    >
                      <span className="ck">{on && <Icon n="check" s={12} />}</span>
                      <span className="ec">{item.enc}</span>
                      {item.descKey && <span className="ed">{T(item.descKey)}</span>}
                    </button>
                  );
                })}
              </div>
            );
          })}
        </div>
        <div className="encfoot">
          <div>{T('enc.hintKeys')}</div>
          <div className="k">{T('enc.note')}</div>
        </div>
      </div>
    </>
  );
}
