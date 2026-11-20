import type { ReactNode } from 'react';
import { Icon } from './Icon';

/** 工具栏按钮（.tbtn） */
export function Button({ icon, children, primary, on, disabled, title, onClick }: {
  icon?: string; children?: ReactNode; primary?: boolean; on?: boolean; disabled?: boolean;
  title?: string; onClick?: () => void;
}) {
  return (
    <button className={'tbtn ' + (primary ? 'primary ' : '') + (on ? 'on ' : '')}
      title={title} disabled={disabled} onClick={onClick}>
      {icon && <Icon n={icon} s={13} />}{children}
    </button>
  );
}
