// 覆盖层宿主：同一时刻最多一个 overlay
import { useAppStore } from '../../store/useAppStore';
import { HostKeyModal } from './HostKeyModal';
import { ConfirmDialog } from './ConfirmDialog';
import { InputDialog } from './InputDialog';
import { PasswordModal } from './PasswordModal';
import { CommandPalette } from './CommandPalette';
import { ContextMenu } from './ContextMenu';
import { CoverageSheet } from './CoverageSheet';
import { HostFormSheet } from '../hosts/HostFormSheet';
import { HostDetailSheet } from '../hosts/HostDetailSheet';
import { ImportWizard } from '../hosts/ImportWizard';
import { SmallFormSheet } from './SmallFormSheet';
import { SettingsSheet } from '../settings/SettingsSheet';
import { BackupSheet } from '../backup/BackupSheet';
import { ConnInfoSheet } from '../metrics/ConnInfoSheet';

export function OverlayHost() {
  const overlay = useAppStore((s) => s.overlay);
  if (!overlay) return null;
  if (overlay.kind === 'sheet') {
    switch (overlay.id) {
      case 'hostform': return <HostFormSheet />;
      case 'hostdetail': return <HostDetailSheet arg={overlay.arg} />;
      case 'import': return <ImportWizard />;
      case 'backup': return <BackupSheet />;
      case 'settings': return <SettingsSheet />;
      case 'coverage': return <CoverageSheet />;
      case 'conninfo': return <ConnInfoSheet />;
      case 'small': return <SmallFormSheet />;
      default: return null;
    }
  }
  if (overlay.kind === 'modal') {
    if (overlay.id === 'hostkey') return <HostKeyModal />;
    if (overlay.id === 'confirm') return <ConfirmDialog />;
    if (overlay.id === 'input') return <InputDialog />;
    if (overlay.id === 'password') return <PasswordModal />;
    return null;
  }
  if (overlay.kind === 'palette') return <CommandPalette />;
  if (overlay.kind === 'menu') return <ContextMenu />;
  return null;
}
