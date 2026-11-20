/** 开关（.switch）。禁用时只降透明度 + aria-disabled，不挂 data-act */
export function Switch({ on, onToggle, label, disabled, act, arg }: {
  on: boolean;
  onToggle?: () => void;
  label?: string;
  disabled?: boolean;
  /** 可选的 data-act / data-arg（供自动化与事件委托定位；禁用时不挂） */
  act?: string;
  arg?: string;
}) {
  return (
    <div role="switch" aria-checked={on} aria-label={label}
      aria-disabled={disabled ? true : undefined}
      className={'switch' + (on ? ' on' : '') + (disabled ? ' off' : '')}
      // 键盘可达：div 本身不可聚焦，补 tabIndex + Space/Enter 切换（否则设置面板全部开关不可用键盘操作）
      tabIndex={disabled ? undefined : 0}
      onKeyDown={(e) => {
        if (disabled) return;
        if (e.key === ' ' || e.key === 'Enter') {
          e.preventDefault();
          onToggle?.();
        }
      }}
      data-act={disabled ? undefined : act}
      data-arg={disabled || !act ? undefined : arg}
      onClick={() => { if (!disabled) onToggle?.(); }} />
  );
}
