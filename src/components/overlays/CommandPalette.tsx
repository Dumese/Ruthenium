// 命令面板：模糊匹配主机 + 片段 + 动作
// 输入用局部更新不整窗重绘；重绘时加 .noanim 并恢复焦点到末尾
// 固定项「快速连接」选中后把输入框切成 user@host[:port] 解析模式，非法输入给提示。
import { useEffect, useMemo, useRef, useState } from 'react';
import { Icon } from '../ui/Icon';
import { useAppStore } from '../../store/useAppStore';
import { filterPal, type PalItem } from '../../lib/palette';
import { parseQuickConnect } from '../../lib/quickConnect';
import { isDevOrE2E } from '../../lib/buildMode';
import { T, useLang } from '../../i18n';

export function palItems(hosts: { id: string; name: string; addr: string; group: string; user: string }[],
                          snips: { id: string; name: string; cmd: string }[]): PalItem[] {
  const items: PalItem[] = [];
  hosts.forEach((h) => items.push({ g: T('pal.gHost'), i: 'box', t: h.name, s: h.user + '@' + h.addr + ' · ' + h.group, act: 'connect', arg: h.id, u: true }));
  snips.forEach((n) => items.push({ g: T('pal.gSnip'), i: 'code', t: n.name, s: n.cmd, act: 'snipinsert', arg: n.id, u: true }));
  const acts: [string, string, string][] = [
    [T('pal.quick'), T('pal.quickSub'), 'quick'],
    [T('pal.newHost'), T('pal.newHostSub'), 'hostform'],
    [T('pal.import'), T('pal.importSub'), 'import'],
    [T('pal.sftp'), T('pal.sftpSub'), 'tabsftp'],
    [T('pal.backup'), T('pal.backupSub'), 'backup'],
    [T('pal.settings'), T('pal.settingsSub'), 'settings'],
    [T('pal.theme'), T('pal.themeSub'), 'theme'],
    [T('pal.side'), T('pal.sideSub'), 'sidebar'],
    [T('pal.focus'), T('pal.focusSub'), 'focus'],
    // 覆盖表是 dev/QA 演示入口（会触发删除主机、关闭全部会话等真实动作），
    // 生产构建不暴露；`goto('coverage')` 在 store 里也做了同样的门控。
    ...(isDevOrE2E
      ? ([[T('pal.cov'), T('pal.covSub'), 'coverage']] as [string, string, string][])
      : []),
    [T('pal.ci'), T('pal.ciSub'), 'conninfo'],
    [T('pal.navx'), T('pal.navxSub'), 'setnavx'],
    [T('pal.navclassic'), T('pal.navclassicSub'), 'setnavclassic'],
    [T('pal.snipnew'), T('pal.snipnewSub'), 'snipnew'],
    [T('pal.rulenew'), T('pal.rulenewSub'), 'rulenew'],
    [T('pal.closeall'), T('pal.closeallSub'), 'closeall'],
  ];
  acts.forEach((a) => items.push({ g: T('pal.gAct'), i: a[2] === 'quick' ? 'bolt' : 'command', t: a[0], s: a[1], act: 'palrun', arg: a[2] }));
  return items;
}

export function CommandPalette() {
  useLang();
  const hosts = useAppStore((s) => s.hosts);
  const snips = useAppStore((s) => s.snippets);
  const palQuery = useAppStore((s) => s.palQuery);
  const palSel = useAppStore((s) => s.palSel);
  const dispatch = useAppStore((s) => s.dispatch);
  const [q, setQ] = useState(palQuery);
  const [sel, setSel] = useState(palSel);
  // quick 模式（固定项「快速连接」被选中后）与非法输入提示
  const [quick, setQuick] = useState(false);
  const [bad, setBad] = useState(false);
  const inputRef = useRef<HTMLInputElement | null>(null);
  const listRef = useRef<HTMLDivElement | null>(null);

  const results = useMemo(() => (quick ? [] : filterPal(palItems(hosts, snips), q)), [hosts, snips, q, quick]);

  useEffect(() => {
    // 恢复输入焦点与光标位置到末尾
    const el = inputRef.current;
    if (el && document.activeElement !== el) { el.focus(); el.setSelectionRange(el.value.length, el.value.length); }
  }, [quick]);

  useEffect(() => {
    if (sel >= results.length) setSel(0);
  }, [results.length, sel]);

  useEffect(() => {
    const el = listRef.current?.querySelector('.palrow.on');
    el?.scrollIntoView({ block: 'nearest' });
  }, [sel, q]);

  const run = (r: PalItem) => {
    // 「快速连接」不关面板，而是切到输入解析模式
    if (r.act === 'palrun' && r.arg === 'quick') {
      setQuick(true);
      setQ('');
      setBad(false);
      setSel(0);
      return;
    }
    dispatch('palrun', r.act + '|' + (r.arg ?? ''));
  };

  const runQuick = () => {
    if (!parseQuickConnect(q)) { setBad(true); return; }
    setBad(false);
    void useAppStore.getState().quickConnect(q);
  };

  return (
    <div className={'palette' + ((q || quick) ? ' noanim' : '')} data-quick={quick ? '1' : undefined}>
      <div className="palinput">
        <Icon n={quick ? 'bolt' : 'search'} s={17} />
        <input
          ref={inputRef}
          className={bad ? 'bad' : undefined}
          data-quick={quick ? 'input' : undefined}
          placeholder={quick ? T('pal.quickPh') : T('pal.ph')}
          autoComplete="off"
          spellCheck={false}
          value={q}
          onChange={(e) => { setQ(e.target.value); setSel(0); setBad(false); }}
          onKeyDown={(e) => {
            if (e.key === 'ArrowDown') { e.preventDefault(); if (!quick) setSel(Math.min(results.length - 1, sel + 1)); }
            else if (e.key === 'ArrowUp') { e.preventDefault(); if (!quick) setSel(Math.max(0, sel - 1)); }
            else if (e.key === 'Enter') {
              e.preventDefault();
              if (quick) { runQuick(); return; }
              const r = results[sel]; if (r) run(r);
            } else if (e.key === 'Escape') { e.preventDefault(); useAppStore.getState().closeOverlay(); }
          }}
        />
        <span className="kbd">ESC</span>
      </div>
      {quick ? (
        <div className="palquick" data-quick="panel">
          <div className="pqhead">{T('pal.quick')}</div>
          <div className={'quickhint' + (bad ? ' err' : '')} data-quick="hint">{bad ? T('pal.quickBad') : T('pal.quickHint')}</div>
        </div>
      ) : (
        <div className="pallist" ref={listRef}>
          {(() => {
            const rows: React.ReactNode[] = [];
            let g = '';
            results.forEach((r, i) => {
              if (r.g !== g) { g = r.g; rows.push(<div className="palgroup" key={'g' + g}>{g}</div>); }
              rows.push(
                <div key={r.act + r.arg + i}
                  className={'palrow' + (i === sel ? ' on' : '')}
                  data-user={r.u ? '1' : undefined}
                  onMouseEnter={() => setSel(i)}
                  onClick={() => run(r)}>
                  <span className="pic"><Icon n={r.i} s={14} /></span>
                  <span className="pt">{r.t}</span>
                  <span className="ps">{r.s}</span>
                </div>,
              );
            });
            if (!results.length) rows.push(<div className="emptyhint" key="none">{T('pal.noMatch')}</div>);
            return rows;
          })()}
        </div>
      )}
      <div className="palfoot">
        <span>{T('pal.sel')}</span><span>{T('pal.run')}</span><span>{T('pal.esc')}</span>
        <span style={{ flex: 1 }} />
        <span>{T('pal.results', { 0: quick ? 1 : results.length })}</span>
      </div>
    </div>
  );
}
