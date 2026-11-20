import { Icon } from './Icon';

/** Toast（.toast） */
export function Toast({ msg }: { msg: string }) {
  return (
    <div className="toast">
      <Icon n="check" s={14} />
      <span>{msg}</span>
    </div>
  );
}
