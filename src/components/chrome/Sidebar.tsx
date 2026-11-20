// 唯一左侧栏：搜索框 + 分段 + 列表 + 底部工具
import { useEffect, useState } from 'react';
import { Icon } from '../ui/Icon';
import { Segmented } from '../ui/Segmented';
import { IconButton } from '../ui/IconButton';
import { useAppStore } from '../../store/useAppStore';
import { HostList } from '../hosts/HostList';
import { SnippetList } from '../snippets/SnippetList';
import { RuleList } from '../tunnels/RuleList';
import { PeerPanel } from '../peers/PeerPanel';
import { T, useLang } from '../../i18n';
import { isDevOrE2E } from '../../lib/buildMode';
import { filterHosts } from '../../store/selectors';
import type { SideTab } from '../../types/models';

function SearchBox() {
  useLang();
  const search = useAppStore((s) => s.search);
  const setSearch = useAppStore((s) => s.setSearch);
  const sideTab = useAppStore((s) => s.sideTab);
  // 局部组件状态 + 受控值同步，输入时只重渲染列表 DOM，不丢焦点
  const [v, setV] = useState(search);
  // store 的 search 会在切换分段（setSideTab 清空）或外部动作时变化：
  // 组件本地值必须跟随，否则输入框显示旧词但列表已不过滤（显示与行为撕裂）。
  useEffect(() => { setV(search); }, [search]);
  const placeholder = T(sideTab === 'hosts' ? 'side.searchHosts' : sideTab === 'snips' ? 'side.searchSnips' : sideTab === 'rules' ? 'side.searchRules' : 'side.searchPeers');
  return (
    <div className="sidesearch">
      <Icon n="search" s={13} />
      <input
        id="sideSearch"
        placeholder={placeholder}
        value={v}
        autoComplete="off"
        onChange={(e) => { setV(e.target.value); setSearch(e.target.value); }}
        onKeyDown={(e) => {
          // Esc 清空搜索；↵ 直接连接 / 插入第一个匹配项
          if (e.key === 'Escape') {
            e.preventDefault();
            setV('');
            setSearch('');
            return;
          }
          if (e.key === 'Enter') {
            e.preventDefault();
            const st = useAppStore.getState();
            if (st.sideTab === 'hosts') {
              const first = filterHosts(st.hosts, st.search)[0];
              // 必须走统一连接闸门 requestConnect（密码闸门 / 会话去重 / 指纹流程都在里面），
              // 直接 connect() 会先建一个注定失败的临时标签、多一次握手。
              if (first) void st.requestConnect(first.id, 'term');
            } else if (st.sideTab === 'snips') {
              const q = st.search.toLowerCase();
              const first = st.snippets.find((x) => !q || (x.name + x.cmd + x.tag).toLowerCase().includes(q));
              if (first) void st.insertSnippet(first.id);
            } else if (st.sideTab === 'rules') {
              const q = st.search.toLowerCase();
              const first = st.tunnels.find((x) => !q || (x.name + x.local + x.remote).toLowerCase().includes(q));
              if (first) void st.toggleRule(first.id);
            }
            // devices 页没有“回车执行”的语义：不要把 Enter 落到转发规则上。
          }
        }}
      />
      <span style={{ fontSize: 10, color: 'var(--t4)' }}>Ctrl+F</span>
    </div>
  );
}

function SideFoot() {
  useLang();
  const dispatch = useAppStore((s) => s.dispatch);
  return (
    <div className="sidefoot">
      <IconButton icon="download" title={T('side.importTitle')} size={15} onClick={() => dispatch('import')} />
      <IconButton icon="layers" title={T('side.backupTitle')} size={15} onClick={() => dispatch('backup')} />
      {/* 覆盖表是演示/QA 入口（可凭空构造指纹弹窗、删除演示数据），
          生产构建不应暴露；只保留在 dev / e2e。 */}
      {isDevOrE2E && (
        <IconButton icon="info" title={T('side.coverageTitle')} size={15} onClick={() => dispatch('coverage')} />
      )}
      <span className="sp" />
    </div>
  );
}

export function Sidebar() {
  useLang();
  const sideTab = useAppStore((s) => s.sideTab);
  const setSideTab = useAppStore((s) => s.setSideTab);
  return (
    // 整条侧栏都归入 data-ctx="side"：搜索框旁 / 分段控件 / 空白处右键都弹侧栏菜单，
    // 而不是让 WebView2 自带菜单漏出来（主机行等更具体的区域仍由更近的 data-ctx 覆盖）。
    <aside className="sidebar" data-ctx="side">
      <div className="sidehead">
        <SearchBox />
        <Segmented<SideTab>
          value={sideTab}
          onChange={setSideTab}
          options={[
            { id: 'hosts', label: T('side.tabHosts'), icon: 'box' },
            { id: 'snips', label: T('side.tabSnips'), icon: 'code' },
            { id: 'rules', label: T('side.tabRules'), icon: 'network' },
            { id: 'peers', label: T('side.tabPeers'), icon: 'layers' },
          ]}
        />
      </div>
      <div className="sidelist" id="sideList" data-ctx="side">
        {sideTab === 'hosts' && <HostList />}
        {sideTab === 'snips' && <SnippetList />}
        {sideTab === 'rules' && <RuleList />}
        {sideTab === 'peers' && <PeerPanel />}
      </div>
      <SideFoot />
    </aside>
  );
}
