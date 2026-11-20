// 右键菜单渲染：唯一一套菜单组件，六类区域共用，条目由 lib/menus.ts 提供。
// 尺寸 208–300 / 条目 30 / 分隔线上下 4 / 圆角 10；按条目数估高后夹进视口（四边 8），
// 贴右下角自动向左上展开，且不压住状态栏文字。
// 键盘：↑↓ 在可用项间循环、↵ 执行、Esc 关闭；打开时不预选任何项。
import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { Icon } from '../ui/Icon';
import { useAppStore } from '../../store/useAppStore';
import type { MenuItem } from '../../types/models';

const GAP = 8;          // 视口四边留白
const ITEM_H = 30;      // 条目高
const DIV_H = 9;        // 分隔线：1px 线 + 上下各 4px
const PAD = 10;         // 面板上下 padding（5px × 2）
const EST_W = 264;      // 估宽（openMenu 用的值）

/** 可用的纵向下边界：状态栏可见时不许压住它（否则会盖住状态栏文字） */
function bottomLimit(): number {
  const sb = document.querySelector('.statusbar') as HTMLElement | null;
  const r = sb?.getBoundingClientRect();
  return r && r.height > 0 ? r.top : window.innerHeight;
}

function clampTo(x: number, y: number, w: number, h: number): { left: number; top: number } {
  const maxX = window.innerWidth - GAP;
  const maxY = bottomLimit() - GAP;
  return {
    left: Math.max(GAP, Math.min(x, maxX - w)),
    top: Math.max(GAP, Math.min(y, maxY - h)),
  };
}

function estimateHeight(items: MenuItem[]): number {
  return PAD + items.reduce((a, it) => a + (it.div ? DIV_H : ITEM_H), 0);
}

export function ContextMenu() {
  const overlay = useAppStore((s) => s.overlay);
  if (!overlay || overlay.kind !== 'menu') return null;
  // key 保证每次打开都是新实例：不预选、位置重新计算
  return <MenuSurface key={overlay.x + ':' + overlay.y + ':' + overlay.items.length} x={overlay.x} y={overlay.y} items={overlay.items} />;
}

function MenuSurface({ x, y, items }: { x: number; y: number; items: MenuItem[] }) {
  const ref = useRef<HTMLDivElement | null>(null);
  const [sel, setSel] = useState(-1);                                   // 打开时不预选
  const [pos, setPos] = useState(() => clampTo(x, y, EST_W, estimateHeight(items)));

  // 挂载后按真实尺寸再夹一次，保证任何情况下都不出屏 / 不压状态栏。
  // 用 offsetWidth/offsetHeight 而不是 getBoundingClientRect：入场动画的 scale 会让
  // rect 偏小，按偏小的尺寸夹边会让动画结束后溢出视口 ~0.3px。
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const next = clampTo(x, y, el.offsetWidth, el.offsetHeight);
    setPos((p) => (Math.abs(p.left - next.left) < 0.5 && Math.abs(p.top - next.top) < 0.5 ? p : next));
  }, [x, y, items]);

  // 点外部 / 窗口尺寸变化 → 关闭
  useEffect(() => {
    const onDown = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) useAppStore.getState().closeOverlay();
    };
    const onResize = () => useAppStore.getState().closeOverlay();
    window.addEventListener('mousedown', onDown, true);
    window.addEventListener('contextmenu', onDown, true);
    window.addEventListener('resize', onResize);
    return () => {
      window.removeEventListener('mousedown', onDown, true);
      window.removeEventListener('contextmenu', onDown, true);
      window.removeEventListener('resize', onResize);
    };
  }, []);

  // 键盘：↑↓ 在可用项之间循环、↵ 执行（先关菜单再执行动作）
  useEffect(() => {
    const enabled = items.map((it, i) => (!it.div && !it.disabled ? i : -1)).filter((i) => i >= 0);
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
        e.preventDefault();
        e.stopPropagation();
        if (!enabled.length) return;
        const cur = enabled.indexOf(sel);
        if (e.key === 'ArrowDown') setSel(cur < 0 ? enabled[0] : enabled[(cur + 1) % enabled.length]);
        else setSel(cur < 0 ? enabled[enabled.length - 1] : enabled[(cur - 1 + enabled.length) % enabled.length]);
        return;
      }
      if (e.key === 'Enter') {
        e.preventDefault();
        e.stopPropagation();
        // 未预选时执行第一个可用项（menuHTML 键盘行为一致）
        const it = items[sel] && !items[sel].div && !items[sel].disabled ? items[sel] : items[enabled[0]];
        if (!it) return;
        runItem(it);
      }
      // Esc 由 useHotkeys 统一关闭 overlay
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  }, [items, sel]);

  return (
    <div ref={ref} className="menu" role="menu" style={{ left: pos.left, top: pos.top }}>
      {items.map((it, i) => {
        if (it.div) return <div className="menudiv" role="separator" key={'d' + i} />;
        const dis = !!it.disabled;
        return (
          <button
            key={i}
            type="button"
            role="menuitem"
            className={'menuitem' + (it.danger ? ' danger' : '') + (dis ? ' off' : '') + (i === sel ? ' on' : '')}
            {...(dis
              ? { 'aria-disabled': 'true' as const }
              : { 'data-act': it.act, 'data-arg': it.arg ?? '' })}
            onClick={dis ? undefined : () => runItem(it)}
            onMouseEnter={dis ? undefined : () => setSel(i)}
          >
            <Icon n={it.i || 'chev'} s={13} />
            <span className="sp">{it.t}</span>
            {it.k && <span className="kbd">{it.k}</span>}
          </button>
        );
      })}
    </div>
  );
}

/** 先关菜单，再执行动作（不许点完还留着菜单） */
function runItem(it: MenuItem): void {
  const s = useAppStore.getState();
  s.closeOverlay();
  if (it.act) s.dispatch(it.act, it.arg ?? '');
}
