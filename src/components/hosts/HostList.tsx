// 侧栏「主机」分组列表（sideListHTML 的 hosts 分支）
// 不设「最近会话」区；单击只选中，双击 / hover ▶ 连接。
import { useMemo } from 'react';
import { useAppStore } from '../../store/useAppStore';
import { foldersOf, filterHosts } from '../../store/selectors';
import { GroupRow } from './GroupRow';
import { HostItem } from './HostItem';
import { useHostDrag } from './useHostDrag';
import { T, useLang } from '../../i18n';
import type { Host, Session } from '../../types/models';

/** 单主机在侧栏里的派生状态（连接数 / 圆点）。 */
function hostDot(hsess: Session[], known: boolean): 'on' | 'busy' | 'off' | 'warn' {
  // 连接中优先显示 busy，其次已连接，最后回落到主机 known/unknown 状态
  if (hsess.some((s) => s.status === 'connecting')) return 'busy';
  if (hsess.some((s) => s.status === 'connected')) return 'on';
  return known ? 'off' : 'warn';
}

export function HostList() {
  useLang();
  const hosts = useAppStore((s) => s.hosts);
  const sessions = useAppStore((s) => s.sessions);
  const active = useAppStore((s) => s.active);
  const selectedHost = useAppStore((s) => s.selectedHost);
  const search = useAppStore((s) => s.search);
  const closedGroups = useAppStore((s) => s.closedGroups);
  const showDots = useAppStore((s) => s.showDots);
  const dispatch = useAppStore((s) => s.dispatch);
  // 显式配置的分组（hosts.toml 的 folders）：即使暂时没有主机也要在侧栏列出来，
  // 否则刚「新增分组」会发现侧栏什么都没发生。
  const cfgFolders = useAppStore((s) => s.folders);
  // 拖拽换分组：pointer 事件自绘（不用 HTML5 DnD，原因见 useHostDrag.ts 顶部注释）
  const moveHostToGroup = useAppStore((s) => s.moveHostToGroup);
  const drag = useHostDrag(moveHostToGroup);

  // 行结构只在「主机 / 会话 / 搜索 / 分组折叠 / 选中」变化时重建：
  // 否则每次渲染都要为每台主机再 filter 一遍 sessions（O(hosts×sessions)），
  // 且 status 抖动会重复构建整棵列表。
  const { rows, total, rendered } = useMemo(() => {
    const folders = foldersOf();
    // 手改/导入的主机可能引用一个不在 folders 里的 group；不能因此让它
    // 从侧栏消失。把这些组追加到渲染列表（颜色为空）。
    const known = new Set(folders.map((f) => f.name));
    for (const h of hosts) {
      const g = h.group.trim();
      if (g && !known.has(g)) {
        folders.push({ name: g, color: '' });
        known.add(g);
      }
    }
    const configured = new Set(cfgFolders.map((f) => f.name));
    const showEmptyGroups = !search.trim();
    const filtered = filterHosts(hosts, search);
    // 一次遍历建立 host → sessions 索引
    const byHost = new Map<string, Session[]>();
    for (const s of sessions) {
      if (!s.hostId) continue;
      const arr = byHost.get(s.hostId);
      if (arr) arr.push(s);
      else byHost.set(s.hostId, [s]);
    }
    const activeSession = sessions.find((s) => s.id === active) ?? null;
    const out: React.ReactNode[] = [];
    let count = 0;
    let rendered = 0;
    folders.forEach((g) => {
      const hs = filtered.filter((x) => x.group === g.name);
      // 空分组：只有「显式配置过」且没在搜索时才渲染（搜索时空分组是噪音）
      const emptyConfigured = showEmptyGroups && configured.has(g.name);
      if (!hs.length && !emptyConfigured) return;
      count += hs.length;
      rendered += 1;
      const closed = !!closedGroups[g.name];
      out.push(
        <GroupRow key={'g:' + g.name} name={g.name} count={hs.length} closed={closed}
          onToggle={() => dispatch('group', g.name)} />,
      );
      if (closed) return;
      hs.forEach((x: Host) => {
        const hsess = byHost.get(x.id) ?? [];
        out.push(
          <HostItem key={x.id} host={x} openCount={hsess.length} selected={selectedHost === x.id}
            current={activeSession?.hostId === x.id} showDot={showDots} dotState={hostDot(hsess, x.known)}
            onSelect={() => { if (!drag.consumeClickSuppress()) dispatch('host', x.id); }}
            onConnect={() => { if (!drag.consumeClickSuppress()) dispatch('connect', x.id); }}
            onDragPointerDown={(ev) => drag.onPointerDown(ev, { id: x.id, name: x.name, group: x.group })} />,
        );
      });
    });
    return { rows: out, total: count, rendered };
  }, [hosts, sessions, active, selectedHost, search, closedGroups, showDots, dispatch, cfgFolders]);

  return (
    <>
      {rows}
      {!total && !rendered && <div className="emptyhint">{T('side.noHost')}<br />{T('side.noHostHint')}</div>}
    </>
  );
}
