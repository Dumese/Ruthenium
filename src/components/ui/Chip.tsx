import type { ReactNode } from 'react';

/** 标签 chip（.chip，tag 决定配色） */
export function Chip({ children, tag }: { children: ReactNode; tag?: string }) {
  return <span className={'chip' + (tag ? ' ' + tag : '')}>{children}</span>;
}
