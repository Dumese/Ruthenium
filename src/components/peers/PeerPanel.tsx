// 侧栏「设备」页：已配对设备（授权管理/文件互传/推送配置/解除）+ 已发现设备（配对）
// + 配置收件箱（导入/忽略）。发现与配对结果由 `peer://discovered` / `peer://pairing` 驱动，
// 收件箱由 `peer://inbox` 驱动（App.tsx 订阅）。
import { useEffect, useState } from 'react';
import { useAppStore } from '../../store/useAppStore';
import { T, useLang } from '../../i18n';
import { Button } from '../ui/Button';
import { Icon } from '../ui/Icon';
import * as api from '../../ipc/commands';
import type { PeerGrants } from '../../ipc/commands';

function grantsOf(p: { grantFiles: boolean; grantControl: boolean; grantConfig: boolean; grantScreen: boolean; grantClipboard: boolean; grantAudio: boolean }): PeerGrants {
  return { files: p.grantFiles, control: p.grantControl, config: p.grantConfig, screen: p.grantScreen, clipboard: p.grantClipboard, audio: p.grantAudio };
}

/** 字节数 → 可读大小（收件箱条目展示用）。 */
function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  return `${(n / 1024 / 1024).toFixed(1)} MB`;
}

export function PeerPanel() {
  useLang();
  const discovered = useAppStore((s) => s.discovered);
  const peers = useAppStore((s) => s.peers);
  const inbox = useAppStore((s) => s.inbox);
  const refreshDiscovered = useAppStore((s) => s.refreshDiscovered);
  const refreshPeers = useAppStore((s) => s.refreshPeers);
  const refreshPeerGrants = useAppStore((s) => s.refreshPeerGrants);
  const refreshInbox = useAppStore((s) => s.refreshInbox);
  const pairStart = useAppStore((s) => s.pairStart);
  const pairWaiting = useAppStore((s) => s.pairWaiting);
  const pairCancelWaiting = useAppStore((s) => s.pairCancelWaiting);
  const updatePeerGrants = useAppStore((s) => s.updatePeerGrants);
  const openPeerSftp = useAppStore((s) => s.openPeerSftp);
  const openPeerTerminal = useAppStore((s) => s.openPeerTerminal);
  const openPeerScreen = useAppStore((s) => s.openPeerScreen);
  const sendPeerConfig = useAppStore((s) => s.sendPeerConfig);
  const deleteInbox = useAppStore((s) => s.deleteInbox);
  const askConfirm = useAppStore((s) => s.askConfirm);
  const showToast = useAppStore((s) => s.showToast);
  const [addr, setAddr] = useState('');
  const [audit, setAudit] = useState<api.AuditEntry[]>([]);

  useEffect(() => {
    // 授权快照只在部分连接时更新：进设备页时刷新一次，之后 60s/窗口聚焦时再刷新
    void refreshPeers().then(() => void refreshPeerGrants());
    const grantsTimer = setInterval(() => { void refreshPeerGrants(); }, 60_000);
    const onFocus = () => { void refreshPeerGrants(); };
    window.addEventListener('focus', onFocus);
    void refreshDiscovered();
    void refreshInbox();
    // 发现列表轮询：后端只在窗口期内发事件，这里兜底刷新（3s 与广播周期一致）。
    // 停留在设备页即视为「需要被看见」，窗口过期后会按需重开；离开页面即停止。
    const timer = setInterval(() => { void refreshDiscovered(); }, 3000);
    // 收件箱兜底轮询：即使事件失败/错过（窗口最小化等），停留在设备页也能看到新条目
    const inboxTimer = setInterval(() => { void refreshInbox(); }, 5000);
    // 审计记录：进入页面时拉一次，之后 10s 兜底刷新（会话结束时后端只落盘）
    const loadAudit = () => { void api.peerAuditRecent(20).then(setAudit).catch(() => { /* ignore */ }); };
    loadAudit();
    const auditTimer = setInterval(loadAudit, 10000);
    return () => { clearInterval(timer); clearInterval(inboxTimer); clearInterval(auditTimer); clearInterval(grantsTimer); window.removeEventListener('focus', onFocus); };
  }, [refreshDiscovered, refreshPeers, refreshPeerGrants, refreshInbox]);

  const unpaired = discovered.filter((d) => !d.paired);
  return (
    <div className="peerspanel">
      <div className="peers-add">
        <input
          className="inp"
          data-peer-addr
          placeholder={T('peers.manualPh')}
          value={addr}
          onChange={(e) => setAddr(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === 'Enter') {
              const a = addr.trim();
              if (a) { setAddr(''); void pairStart(undefined, a); }
            }
          }}
        />
        <Button
          icon="link"
          onClick={() => {
            const a = addr.trim();
            if (!a) { showToast(T('peers.manualEmpty')); return; }
            setAddr('');
            void pairStart(undefined, a);
          }}
        >
          {T('peers.manualPair')}
        </Button>
      </div>

      {pairWaiting.length > 0 && (
        <>
          <div className="peers-sec">{T('peers.pairWaiting')}</div>
          {pairWaiting.map((w) => (
            <div key={w.pairingId} className="peerrow peerrow-wait" data-pair-wait-id={w.pairingId}>
              <div className="pr-main">
                <div className="pr-name">{w.peerName}</div>
                <div className="pr-sub mono">{T('peers.pairWaitingHint', { 0: w.peerAddr })}</div>
              </div>
              <button className="tbtn" data-pair-wait-cancel onClick={() => pairCancelWaiting(w.pairingId)}>
                {T('peers.pairCancel')}
              </button>
            </div>
          ))}
        </>
      )}

      {inbox.length > 0 && (
        <>
          <div className="peers-sec">{T('inbox.title')}</div>
          {inbox.map((it) => (
            <div key={it.id} className="inboxrow" data-inbox-id={it.id}>
              <div className="pr-main">
                <div className="pr-name">
                  {T('inbox.item', { 0: it.from, 1: String(it.hosts) })}
                </div>
                <div className="pr-sub mono">
                  {it.receivedAt} · {fmtBytes(it.bytes)}
                  {!it.valid && it.error ? ` · ${it.error}` : ''}
                </div>
              </div>
              <div className="inbox-acts">
                <button
                  className="tbtn"
                  data-inbox-import
                  disabled={!it.valid}
                  onClick={() => askConfirm('importinbox', it.id, T('inbox.import'), T('inbox.importWarn'))}
                >
                  {T('inbox.import')}
                </button>
                <button className="tbtn" data-inbox-dismiss onClick={() => void deleteInbox(it.id)}>
                  {T('inbox.dismiss')}
                </button>
              </div>
            </div>
          ))}
        </>
      )}

      {peers.length > 0 && (
        <>
          <div className="peers-sec">{T('peers.paired')}</div>
          {peers.map((p) => (
            <div key={p.id} className="peerrow peerrow-paired" data-peer-id={p.id}>
              <div className="pr-head">
                <span className="pr-name">{p.name}</span>
                <div className="pr-acts">
                  <button
                    className="iconbtn peer-sftp"
                    data-peer-sftp
                    title={p.peerAllowsFiles ? T('peers.sftp') : T('peers.sftpDenied')}
                    onClick={() => void openPeerSftp(p.id)}
                  >
                    <Icon n="folder" s={15} />
                  </button>
                  <button
                    className="iconbtn peer-control"
                    data-peer-control
                    title={p.peerAllowsControl ? T('peers.control') : T('peers.controlDenied')}
                    onClick={() => void openPeerTerminal(p.id)}
                  >
                    <Icon n="terminal" s={15} />
                  </button>
                  <button
                    className="iconbtn peer-screen"
                    data-peer-screen
                    title={p.peerAllowsScreen ? T('peers.screen') : T('peers.screenDenied')}
                    onClick={() => void openPeerScreen(p.id)}
                  >
                    <Icon n="monitor" s={15} />
                  </button>
                  <button
                    className="iconbtn peer-screen-control"
                    data-peer-screen-control
                    title={p.peerAllowsScreen ? T('peers.screenControl') : T('peers.screenDenied')}
                    onClick={() => void openPeerScreen(p.id, true)}
                  >
                    <Icon n="keyboard" s={15} />
                  </button>
                  <button
                    className="iconbtn peer-sendcfg"
                    data-peer-sendcfg
                    title={p.peerAllowsConfig ? T('peers.sendConfig') : T('peers.sendConfigDenied')}
                    onClick={() => void sendPeerConfig(p.id)}
                  >
                    <Icon n="upload" s={15} />
                  </button>
                  <button
                    className="iconbtn danger peer-remove"
                    data-peer-remove
                    title={T('peers.remove')}
                    onClick={() => askConfirm('peerremove', p.id, T('peers.remove'), T('peers.removeWarn'))}
                  >
                    <Icon n="trash" s={15} />
                  </button>
                </div>
              </div>
              <div className="pr-sub mono">{p.lastAddr ?? ''}</div>
              <div className="pr-grants">
                {(['files', 'control', 'config', 'screen', 'clipboard', 'audio'] as const).map((k) => (
                  <label key={k} className="pr-grant">
                    <input
                      type="checkbox"
                      data-grant={k}
                      checked={grantsOf(p)[k]}
                      onChange={(e) => void updatePeerGrants(p.id, { ...grantsOf(p), [k]: e.target.checked })}
                    />
                    {T(k === 'files' ? 'peers.grantFiles' : k === 'control' ? 'peers.grantControl' : k === 'config' ? 'peers.grantConfig' : k === 'screen' ? 'peers.grantScreen' : k === 'clipboard' ? 'peers.grantClipboard' : 'peers.grantAudio')}
                  </label>
                ))}
              </div>
            </div>
          ))}
        </>
      )}

      <div className="peers-sec">{T('audit.title')}</div>
      {audit.length === 0 && <div className="peers-empty">{T('audit.empty')}</div>}
      {audit.length > 0 && (
        <div className="auditlist" data-audit-list>
          {audit.map((a, i) => (
            <div key={`${a.at}-${i}`} className="auditrow" data-audit-kind={a.kind}>
              <div className="pr-main">
                <div className="pr-name">
                  {a.kind === 'screen' ? T('audit.kindScreen') : a.kind === 'pair' ? T('audit.kindPair') : T('audit.kindControl')}
                  {' · '}{a.peerName}
                </div>
                <div className="pr-sub mono">
                  {a.at} · {a.peerAddr}
                  {a.width && a.height ? ` · ${a.width}×${a.height}` : ''}
                  {a.quality ? ` · q${a.quality}` : ''}
                  {a.frames ? ` · ${a.frames}f` : ''}
                  {a.bytes ? ` · ${fmtBytes(a.bytes)}` : ''}
                  {a.kind !== 'pair' ? ` · ${a.seconds}s` : ''}
                  {a.reason ? ` · ${a.reason}` : ''}
                </div>
              </div>
            </div>
          ))}
          <button
            className="tbtn audit-clear"
            data-audit-clear
            onClick={() => askConfirm('auditclear', '', T('audit.clear'), T('toast.delFileWarn', { 0: T('audit.title') }))}
          >
            {T('audit.clear')}
          </button>
        </div>
      )}

      <div className="peers-sec">{T('peers.discovered')}</div>
      {unpaired.length === 0 && <div className="peers-empty">{T('peers.empty')}</div>}
      {unpaired.map((d) => (
        <div key={d.deviceId} className="peerrow" data-device-id={d.deviceId}>
          <div className="pr-main">
            <div className="pr-name">{d.name}</div>
            <div className="pr-sub mono">{d.pairAddr} · {d.fp}</div>
          </div>
          <button
            className="tbtn"
            data-peer-pair
            disabled={!d.pairing}
            title={d.pairing ? undefined : T('peers.pairUnavailable')}
            onClick={() => void pairStart(d.deviceId)}
          >
            {T('peers.pair')}
          </button>
        </div>
      ))}
    </div>
  );
}
