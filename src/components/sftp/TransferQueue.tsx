// 传输队列：sftp://progress 事件驱动，可取消，完成后刷新两栏
import { Icon } from '../ui/Icon';
import { useAppStore } from '../../store/useAppStore';
import { fmtSize } from '../../lib/format';
import { T, useLang } from '../../i18n';

export function TransferQueue() {
  useLang();
  const transfers = useAppStore((s) => s.transfers);
  const dispatch = useAppStore((s) => s.dispatch);
  const cancelTransfer = useAppStore((s) => s.cancelTransfer);
  const running = transfers.filter((t) => t.state === 'running').length;
  return (
    <div className="queue">
      <div className="qhead">
        <span>{T('sftp.queueTitle')}</span>
        <span className="sp" />
        <span>{T('sftp.queueCount', { 0: transfers.length, 1: running ? T('sftp.transferring') : T('sftp.allDone') })}</span>
        {transfers.length > 0 && (
          <button
            className="qclear"
            disabled={running > 0}
            onClick={() => dispatch('qclear')}
          >{T('sftp.clear')}</button>
        )}
      </div>
      {!transfers.length && (
        <div className="qempty">
          {T('sftp.emptyQueue')}
        </div>
      )}
      {transfers.map((t) => {
        const pct = t.bytesTotal > 0 ? (t.bytesDone / t.bytesTotal) * 100 : 0;
        return (
          <div className="qrow" key={t.id}>
            <Icon n={t.dir === 'up' ? 'upload' : 'download'} s={12} />
            <span className="qn">{t.name}</span>
            <span style={{ color: 'var(--t3)' }}>{t.dir === 'up' ? T('sftp.upShort') : T('sftp.downShort')}</span>
            <span className="qbar">
              <i style={{ ['--p' as string]: Math.max(0, Math.min(1, pct / 100)) } as React.CSSProperties} />
            </span>
            <span className="qpct">
              {t.state === 'error' ? T('err.unknown') : t.state === 'cancelled' ? T('sftp.cancelled') : Math.round(pct) + '%'}
            </span>
            <span style={{ color: 'var(--t4)', fontSize: 10 }}>{fmtSize(t.bytesDone)}</span>
            {t.state === 'running' && <button style={{ color: 'var(--t3)' }} onClick={() => void cancelTransfer(t.id)}>{T('c.cancel')}</button>}
          </div>
        );
      })}
    </div>
  );
}
