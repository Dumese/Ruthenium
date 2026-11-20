import { Icon } from '../ui/Icon';
import { Chip } from '../ui/Chip';
import { T } from '../../i18n';
import type { Host } from '../../types/models';
import type { PointerEvent as ReactPointerEvent } from 'react';

/** 主机行（.hostitem）：单击只选中；双击或 hover ▶ 才连接 */
export function HostItem({ host, openCount, selected, current, showDot, dotState, onSelect, onConnect, onDragPointerDown }: {
  host: Host; openCount: number; selected: boolean; current: boolean; showDot: boolean;
  dotState: 'on' | 'busy' | 'off' | 'warn';
  onSelect: () => void; onConnect: () => void;
  /** 拖拽起点（pointer 事件自绘，见 useHostDrag：不用 HTML5 DnD） */
  onDragPointerDown?: (ev: ReactPointerEvent) => void;
}) {
  return (
    <div
      className={'hostitem' + (selected || current ? ' on' : '')}
      role="button"
      tabIndex={0}
      data-ctx="host"
      data-arg={host.id}
      data-group={host.group}
      onPointerDown={onDragPointerDown}
      onClick={onSelect}
      onDoubleClick={onConnect}
      onKeyDown={(e) => { if (e.key === 'Enter') onConnect(); }}
    >
      {showDot && <span className={'dot ' + dotState} />}
      <span className="hi">
        {/* 两行独立 block */}
        <span className="hname">{host.name}</span>
        <span className="hsub">{host.user}@{host.addr}{host.port && host.port !== 22 ? ':' + host.port : ''}</span>
      </span>
      {openCount > 0 && <Chip>{openCount}</Chip>}
      <span
        className="hact"
        role="button"
        tabIndex={0}
        // 标记为拖拽禁区：主机行 pointerdown 会开始拖拽跟踪，若在 ▶ 上也开始，
        // WebView2 的指针捕获会把随后的 click 重定向到整行，这里就永远收不到点击。
        data-nodrag=""
        title={T('menu.connect')}
        onClick={(e) => { e.stopPropagation(); onConnect(); }}
        onKeyDown={(e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); e.stopPropagation(); onConnect(); } }}
      >
        <Icon n="play" s={12} />
      </span>
    </div>
  );
}
