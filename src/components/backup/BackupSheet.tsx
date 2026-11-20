// 备份与迁移 Sheet。导出 JSON 不含任何秘密；导入前先确认覆盖。
// 桌面版走原生保存 / 打开对话框 + export_backup / import_backup IPC（整体替换）；
// 浏览器 / mock 降级为 Blob 下载 + 隐藏 file input（E2E 可用）。
import { useRef } from 'react';
import { Sheet } from '../overlays/Sheet';
import { Button } from '../ui/Button';
import { useAppStore } from '../../store/useAppStore';
import { isTauri } from '../../ipc/env';
import { pickBackupFile } from '../../lib/dialog';
import { T, useLang } from '../../i18n';

export function BackupSheet() {
  useLang();
  const hosts = useAppStore((s) => s.hosts);
  const snippets = useAppStore((s) => s.snippets);
  const tunnels = useAppStore((s) => s.tunnels);
  const dispatch = useAppStore((s) => s.dispatch);
  const askConfirm = useAppStore((s) => s.askConfirm);
  const showToast = useAppStore((s) => s.showToast);
  const fileRef = useRef<HTMLInputElement | null>(null);
  return (
    <Sheet
      title={T('bk.title')}
      foot={
        <>
          <span className="sp" />
          <Button primary onClick={() => dispatch('closeov')}>{T('c.done')}</Button>
        </>
      }
    >
      <div className="sec" style={{ borderTop: 0, paddingTop: 0 }}>
        <div className="sectitle">{T('bk.export')}</div>
        <div className="dpgrid" style={{ padding: 0, gridTemplateColumns: '1fr' }}>
          <div className="dpcard" style={{ display: 'flex', alignItems: 'center', gap: 12 }}>
            <div style={{ flex: 1 }}>
              <h4>ruthenium-backup.json</h4>
              <p>{T('bk.stats', { 0: hosts.length, 1: snippets.length, 2: tunnels.length })}</p>
            </div>
            <Button icon="download" primary onClick={() => dispatch('export')}>{T('bk.do')}</Button>
          </div>
        </div>
      </div>
      <div className="sec">
        <div className="sectitle">{T('bk.import')}</div>
        <div style={{ marginTop: 10 }}>
          <Button
            icon="upload"
            onClick={() => {
              if (!isTauri) { fileRef.current?.click(); return; }
              // 桌面版：原生打开对话框拿到一次性 token → 确认后 import_backup(token)
              void (async () => {
                try {
                  const picked = await pickBackupFile(T('bk.openTitle'));
                  if (picked) askConfirm('importbackupfile', picked.token, T('bk.import'), T('bk.importBody'));
                } catch (e) {
                  showToast(T('toast.importFailed', { 0: (e as Error).message }));
                }
              })();
            }}
          >
            {T('bk.pick')}
          </Button>
        </div>
        <input
          ref={fileRef}
          type="file"
          id="backupFileInput"
          accept=".json,application/json"
          hidden
          onChange={(e) => {
            const f = e.target.files?.[0];
            if (!f) return;
            const reader = new FileReader();
            reader.onload = () => {
              // 导入前弹 confirm Modal 说明将覆盖
              askConfirm('importbackup', String(reader.result ?? ''), T('bk.import'), T('bk.importBody'));
            };
            reader.readAsText(f);
            e.target.value = '';
          }}
        />
      </div>
      <div className="sec">
        <div className="sectitle">{T('bk.paths')}</div>
        <div className="fingerprint">
          %APPDATA%\Ruthenium\hosts.toml {T('bk.cfgFile')}<br />
          %LOCALAPPDATA%\Ruthenium\logs\ {T('bk.logDir')}<br />
          %USERPROFILE%\.ssh\known_hosts {T('bk.managed')}
        </div>
      </div>
    </Sheet>
  );
}
