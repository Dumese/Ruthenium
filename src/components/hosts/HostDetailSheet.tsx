// 主机详情 Sheet
import { useEffect, useState } from 'react';
import { Sheet } from '../overlays/Sheet';
import { Button } from '../ui/Button';
import { Pill } from '../ui/Pill';
import { useAppStore } from '../../store/useAppStore';
import { sshCommand } from '../../lib/sshConfigParser';
import { tagLabel } from '../../lib/tags';
import { hasPassword } from '../../ipc/commands';
import { T, useLang } from '../../i18n';

export function HostDetailSheet({ arg }: { arg?: string }) {
  useLang();
  const host = useAppStore((s) => s.hosts.find((x) => x.id === arg));
  const sessions = useAppStore((s) => s.sessions);
  const dispatch = useAppStore((s) => s.dispatch);
  // 「已保存 / 未保存」是派生态：来自 has_password，绝不回显密码
  const [saved, setSaved] = useState(false);
  const hostId = host?.id;
  const auth = host?.auth ?? 'key';
  useEffect(() => {
    if (!hostId || auth !== 'password') { setSaved(false); return; }
    let dead = false;
    void hasPassword(hostId).then((v) => { if (!dead) setSaved(v); }).catch(() => { /* keyring 不可用按未保存 */ });
    return () => { dead = true; };
  }, [hostId, auth]);
  if (!host) return null;
  const authText = auth === 'password'
    ? (saved ? T('hd.authPass') : T('hd.authPassNo'))
    : auth === 'agent' ? T('hd.authAgent') : T('auth.key');
  const rows: [string, string][] = [
    [T('hd.addr'), host.addr], [T('hd.user'), host.user], [T('hd.port'), String(host.port || '—')],
    [T('hd.group'), host.group], [T('hd.tag'), tagLabel(host.tag)],
    [T('hd.auth'), authText],
    // 非 key 认证不显示私钥行（没有可用的私钥路径）
    ...(auth === 'key' ? [[T('hd.key'), host.keyRef] as [string, string]] : []),
    [T('hd.verify'), host.known ? T('hd.trusted') : T('hd.needConfirm')],
    [T('hd.status'), sessions.some((s) => s.hostId === host.id) ? T('hd.sessionOpen') : T('vaults.offline')],
  ];
  return (
    <Sheet
      title={host.name}
      sub={host.note || undefined}
      foot={
        <>
          <span className="sp" />
          <Button onClick={() => dispatch('closeov')}>{T('c.close')}</Button>
        </>
      }
    >
      <div className="sec" style={{ borderTop: 0, paddingTop: 0 }}>
        <div className="sectitle">{T('hd.secConn')}</div>
        {rows.map((k) => (
          <div className="kv" key={k[0]}><span>{k[0]}</span><span>{k[1]}</span></div>
        ))}
      </div>
      <div className="sec">
        <div className="sectitle">{T('hd.secCmd')}</div>
        <div className="fingerprint mono">{sshCommand(host)}</div>
        <div style={{ marginTop: 9 }}>
          <Button icon="copy" onClick={() => dispatch('copyssh', host.id)}>{T('hd.copy')}</Button>
        </div>
      </div>
      <div className="sec">
        <div className="sectitle">{T('hd.secOn')}</div>
        <div className="pillrow">
          <Pill on onClick={() => dispatch('connect', host.id)}>{T('hd.openTerm')}</Pill>
          <Pill onClick={() => dispatch('opensftp', host.id)}>{T('hd.openSftp')}</Pill>
          <Pill onClick={() => dispatch('hostedit', host.id)}>{T('hd.editCfg')}</Pill>
          <Pill onClick={() => dispatch('rulenew', host.id)}>{T('hd.newRule')}</Pill>
        </div>
      </div>
    </Sheet>
  );
}
