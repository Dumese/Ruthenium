import { Icon } from './Icon';

/** 分段控件（.seg） */
export function Segmented<T extends string>({ value, options, onChange }: {
  value: T; options: { id: T; label: string; icon?: string }[]; onChange: (id: T) => void;
}) {
  return (
    <div className="seg">
      {options.map((o) => (
        <button key={o.id} className={value === o.id ? 'on' : ''} onClick={() => onChange(o.id)}>
          {o.icon && <Icon n={o.icon} s={13} />}{o.label}
        </button>
      ))}
    </div>
  );
}
