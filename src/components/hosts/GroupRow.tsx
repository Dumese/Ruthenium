import { Icon } from '../ui/Icon';

/** 分组行（.grouprow，可折叠；独立文件，HostItem.tsx 中亦引用同一样式）。
 *  右键弹分组菜单（重命名 / 删除）：data-ctx="group" + data-arg 供 useContextMenu 解析。 */
export function GroupRow({ name, count, closed, onToggle }: {
  name: string; count: number; closed: boolean; onToggle: () => void;
}) {
  return (
    <button
      className={'grouprow' + (closed ? ' closed' : '')}
      data-group={name}
      data-ctx="group"
      data-arg={name}
      // 分组名是用户数据（不是界面文案）：i18n 残检据此跳过，避免把用户起的
      // 中文组名误报成"英文界面中文残留"。
      data-user="1"
      onClick={onToggle}
    >
      <Icon n="down" s={12} className="caret" />
      <span style={{ minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{name}</span>
      <span className="sp" />
      <span className="chip">{count}</span>
    </button>
  );
}
