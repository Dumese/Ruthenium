import type { CSSProperties, ReactNode } from 'react';

/** pill 选择按钮（.pill；style 用于字体 pill 按各自 mono 字体渲染） */
export function Pill({ on, children, onClick, style }: { on?: boolean; children: ReactNode; onClick?: () => void; style?: CSSProperties }) {
  return <button className={'pill' + (on ? ' on' : '')} style={style} onClick={onClick}>{children}</button>;
}
