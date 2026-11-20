import { Icon } from './Icon';

/** data-act 风格图标按钮（.iconbtn） */
export function IconButton({ icon, title, on, danger, size = 16, onClick, className = '' }: {
  icon: string; title: string; on?: boolean; danger?: boolean; size?: number;
  onClick?: (e: React.MouseEvent) => void; className?: string;
}) {
  return (
    <button className={'iconbtn ' + (on ? 'on ' : '') + (danger ? 'danger ' : '') + className}
      title={title} onClick={onClick}>
      <Icon n={icon} s={size} />
    </button>
  );
}
