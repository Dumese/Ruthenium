/** 状态点（.dot） */
export function Dot({ state, size }: { state: 'on'|'off'|'warn'|'busy'; size?: number }) {
  return <span className={'dot ' + state} style={size ? { width: size, height: size } : undefined} />;
}
