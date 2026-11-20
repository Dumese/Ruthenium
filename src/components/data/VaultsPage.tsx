// 主机库数据页
import { Icon } from '../ui/Icon';
import { Dot } from '../ui/Dot';
import { Chip } from '../ui/Chip';
import { Button } from '../ui/Button';
import { useAppStore } from '../../store/useAppStore';
import { T, useLang } from '../../i18n';

/** 凭据列只显示文件名（如 id_ed25519），完整路径放 title（主机库表格） */
function keyName(p: string): string {
  const a = String(p || '').split(/[\\/]/).filter(Boolean);
  return a[a.length - 1] || String(p || '');
}

export function VaultsPage() {
  useLang();
  const hosts = useAppStore((s) => s.hosts);
  const sessions = useAppStore((s) => s.sessions);
  const showDots = useAppStore((s) => s.showDots);
  const selectedHost = useAppStore((s) => s.selectedHost);
  const dispatch = useAppStore((s) => s.dispatch);
  return (
    <div className="datapage">
      <div className="dphead">
        <div>
          <h2>{T('vaults.title')}</h2>
          <p>{T('vaults.sub', { 0: hosts.length })}</p>
        </div>
        <span className="sp" />
        <Button icon="download" onClick={() => dispatch('import')}>{T('vaults.import')}</Button>
        <Button icon="plus" primary onClick={() => dispatch('hostform')}>{T('vaults.new')}</Button>
      </div>
      <div className="dptable-wrap">
        <table className="dptable">
          <thead>
            <tr>
              <th>{T('th.host')}</th><th>{T('th.addr')}</th><th>{T('th.userPort')}</th><th>{T('th.group')}</th><th>{T('th.status')}</th><th>{T('th.cred')}</th>
              <th style={{ textAlign: 'right' }}>{T('th.actions')}</th>
            </tr>
          </thead>
          <tbody>
            {hosts.map((x) => {
              const hs = sessions.filter((s) => s.hostId === x.id);
              const open = hs.length;
              const connecting = hs.some((s) => s.status === 'connecting');
              // 凭据列：key 认证显示私钥文件名；密码 / agent / 本地终端没有私钥路径，显示 —
              const isKey = (x.auth ?? 'key') === 'key' && !!x.keyRef;
              const credTitle = isKey ? x.keyRef : T(x.port === 0 ? 'app.localTerm' : 'auth.' + (x.auth ?? 'key'));
              return (
                <tr
                  key={x.id}
                  className={selectedHost === x.id ? 'on' : undefined}
                  data-ctx="host"
                  data-arg={x.id}
                  onClick={() => dispatch('host', x.id)}
                >
                  <td className="strong">
                    {showDots && <Dot state={open ? 'on' : x.known ? 'off' : 'warn'} />}
                    <span>{x.name}</span>
                  </td>
                  <td className="mono">{x.addr}</td>
                  <td>{x.user} / {x.port || '—'}</td>
                  <td><Chip tag={x.tag}>{x.group}</Chip></td>
                  <td>{connecting ? T('side.connecting') : open ? T('vaults.openTabs', { 0: open }) : T('vaults.offline')}</td>
                  <td className="mono cred" style={{ fontSize: 10.5 }} title={credTitle}>
                    {isKey ? keyName(x.keyRef) : '—'}
                  </td>
                  <td>
                    <div className="acts">
                      <button title={T('menu.connect')} onClick={() => dispatch('connect', x.id)}><Icon n="play" s={13} /></button>
                      <button title={T('menu.openSftp')} onClick={() => dispatch('opensftp', x.id)}><Icon n="folder" s={13} /></button>
                      <button title={T('menu.detail')} onClick={() => dispatch('hostdetail', x.id)}><Icon n="info" s={13} /></button>
                      <button title={T('menu.edit')} onClick={() => dispatch('hostedit', x.id)}><Icon n="edit" s={13} /></button>
                      <button title={T('menu.delHost')} onClick={() => dispatch('hostdel', x.id)}><Icon n="trash" s={13} /></button>
                    </div>
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      <div className="dpgrid">
        <div className="dpcard">
          <h4>{T('vaults.card1t')}</h4>
          <p>{T('vaults.card1b')}</p>
          <span className="mono">%USERPROFILE%\.ssh\config → hosts.toml</span>
          <div className="dprow"><Button onClick={() => dispatch('import')}>{T('vaults.card1btn')}</Button></div>
        </div>
        <div className="dpcard">
          <h4>{T('vaults.card2t')}</h4>
          <p>{T('vaults.card2b')}</p>
          <span>{T('vaults.encNone')}</span>
          <div className="dprow"><Button onClick={() => dispatch('settings')}>{T('vaults.card2btn')}</Button></div>
        </div>
        <div className="dpcard">
          <h4>{T('vaults.card3t')}</h4>
          <p>{T('vaults.card3b')}</p>
          <span>{T('vaults.navPath')}</span>
          <div className="dprow"><Button onClick={() => dispatch('setnavclassic')}>{T('vaults.card3btn')}</Button></div>
        </div>
      </div>
    </div>
  );
}
