// 会话标签栏：nav=x 时含 5 个数据标签；会话标签 + 新建 / SFTP / 断线
import { Icon } from '../ui/Icon';
import { Dot } from '../ui/Dot';
import { useAppStore } from '../../store/useAppStore';
import { useMock } from '../../ipc/env';
import { T, useLang } from '../../i18n';

export function TabBar() {
  useLang();
  const nav = useAppStore((s) => s.nav);
  const sessions = useAppStore((s) => s.sessions);
  const active = useAppStore((s) => s.active);
  const dispatch = useAppStore((s) => s.dispatch);
  const dataTabs: [string, string, string][] = [
    ['vaults', T('tab.vaults'), 'box'],
    ['sftpPage', T('tab.sftp'), 'folder'],
    ['snipsPage', T('tab.snips'), 'code'],
    ['rulesPage', T('tab.rules'), 'network'],
    // 局域网设备页：水平导航下也要有入口，否则只能切回经典侧栏
    ['peers', T('side.tabPeers'), 'layers'],
  ];
  return (
    <div className="tabbar">
      {/* 标签区单独滚动：右侧操作固定在栏尾，标签再多也不会被一起滚出可视区 */}
      <div className="tabbar-scroll">
        {nav === 'x' && (
          <>
            {dataTabs.map(([id, label, icon]) => (
              <button key={id} className={'tab data' + (active === id ? ' on' : '')} onClick={() => dispatch('datatab', id)}>
                <Icon n={icon} s={13} />
                <span className="tn">{label}</span>
              </button>
            ))}
            <span className="sep" style={{ alignSelf: 'center', height: 17 }} />
          </>
        )}
        {sessions.map((s) => (
          <button
            key={s.id}
            className={'tab' + (active === s.id ? ' on' : '')}
            data-status={s.status}
            data-ctx="tab"
            data-arg={s.id}
            title={s.title}
            onClick={() => dispatch('tab', s.id)}
          >
            <Dot state={s.status === 'connecting' ? 'busy' : s.status === 'connected' ? 'on' : 'warn'} size={6} />
            <span className="tn">{s.title}</span>
            <span className="tx" onClick={(e) => { e.stopPropagation(); dispatch('tabclose', s.id); }}>
              <Icon n="x" s={11} />
            </span>
          </button>
        ))}
      </div>
      <div className="tabbar-acts">
        <button className="tabplus" title={T('tab.new')} onClick={() => dispatch('newtab')}><Icon n="plus" s={15} /></button>
        {sessions.length > 0 && (
          <>
            <button className="tabplus" title={T('tab.openSftp')} onClick={() => dispatch('tabsftp')}><Icon n="folder" s={15} /></button>
            {/* 演示用按钮：只在 mock/浏览器调试时出现，正式包不渲染（避免假功能入口） */}
            {useMock && (
              <button className="tabplus" title={T('tab.simDisconnect')} onClick={() => dispatch('disconnect')}><Icon n="wifi" s={15} /></button>
            )}
          </>
        )}
      </div>
    </div>
  );
}
