// 侧栏主机拖拽换分组（pointer 事件自绘，**不用** HTML5 Drag & Drop）。
//
// 为什么不用原生 DnD（这套功能最容易踩的坑）：
//   1. Tauri 窗口默认 dragDropEnabled=true，WebView2 会接 OS 级文件拖放；原生 dragstart
//      在部分 Windows/WebView2 组合上会被窗口的拖放处理抢走，表现为「拖不动 / 拖出窗口」。
//   2. 原生拖拽 dragImage 在 WebView2 里跟随鼠标有偏移，且没法自定义样式。
//   3. 原生 DnD 的 drop 判定依赖 dragover.preventDefault()，漏一处整段失效。
// 因此：pointerdown → 超过阈值进入拖拽 → 自绘 ghost（pointer-events:none）→
// elementFromPoint 命中 `[data-group]` → pointerup 落盘。
//
// 实现时必须遵守的约束：
//   a. 清理要「先清 DOM/监听器，再清 ref」——反过来会让 ghost/高亮永久残留；
//   b. 高亮与拖拽源用 **data-* 属性**而不是 class：React 重渲染会按 props 重写 className，
//      但不会动我们设的未知属性（侧栏会因会话状态刷新而重渲染）；
//   c. 监听器函数必须**稳定引用**：每次渲染新建闭包会让 removeEventListener 失效，
//      泄漏的 keydown 监听器会把全局 Esc 吞掉（让「搜索框 Esc 清空」失效）；
//   d. 拖拽尾随的 click 要吞掉，否则拖完会顺手选中 / 双击连开终端；
//   e. 折叠分组也能当落点，落进去自动展开；
//   f. 指针贴近列表上下边缘自动滚动，滚动后用最后坐标重新命中；
//   g. 同组拖放是 no-op：不写盘、不弹 toast；
//   h. pointer capture 要等拖拽真正开始（超过阈值）再设：pointerdown 就捕获会让
//      WebView2 把同一指针序列的 click 重定向到捕获元素（整行），行内自绘的 ▶
//      （`<span role="button">` + `[data-nodrag]`）单击永远收不到——
//      「点 ▶ 不连接、只有双击才行」。

import { useCallback, useEffect, useRef } from 'react';
import type { PointerEvent as ReactPointerEvent } from 'react';

const THRESHOLD_PX = 5;
const EDGE_PX = 30;
const SCROLL_STEP = 9;

/**
 * 不作为拖拽起点的元素。除了原生控件，还用 `[data-nodrag]` 标记行内的自绘按钮
 * （如 `.hact` ▶ 是 `<span role="button">`，不写进这里就会命中下面的指针捕获副作用）。
 */
const NO_DRAG_SEL = '[data-nodrag],button,a,input,textarea,select,[contenteditable="true"]';

type Active = {
  hostId: string;
  hostName: string;
  from: string;
  x: number;
  y: number;
  pointerId: number;
  started: boolean;
  over: string | null;
  ghost: HTMLDivElement | null;
  srcEl: HTMLElement | null;
  captureEl: HTMLElement | null;
  hlEl: HTMLElement | null;
  scrollDir: -1 | 0 | 1;
  raf: number | null;
  lastX: number;
  lastY: number;
};

type Handlers = {
  move: (ev: PointerEvent) => void;
  up: (ev: PointerEvent) => void;
  cancel: () => void;
  key: (ev: KeyboardEvent) => void;
  blur: () => void;
};

export interface HostDragApi {
  /** 绑到主机行的 onPointerDown。只在左键、且不是连接按钮等交互元素上才开始。 */
  onPointerDown: (ev: ReactPointerEvent, host: { id: string; name: string; group: string }) => void;
  /** 拖拽结束后吞掉紧随其后的 click，避免「拖完顺手选中/连开终端」。 */
  consumeClickSuppress: () => boolean;
  /** 取消当前拖拽（Esc / 窗口失焦 / 卸载时调用）。 */
  cancel: () => void;
}

/** 清掉拖拽期间所有临时 DOM（ghost / 高亮 / 源行淡出）。可重复调用。 */
function cleanupDom(a: Active | null): void {
  a?.ghost?.remove();
  if (a) a.ghost = null;
  a?.hlEl?.removeAttribute('data-drop');
  if (a) a.hlEl = null;
  document.querySelectorAll('[data-drop]').forEach((el) => el.removeAttribute('data-drop'));
  a?.srcEl?.removeAttribute('data-dragsrc');
  if (a) a.srcEl = null;
  document.body.classList.remove('host-dragging');
}

/** 命中测试：ghost 是 pointer-events:none，elementFromPoint 拿到的是下面的真实元素。 */
function hitGroup(x: number, y: number): { group: string | null; el: HTMLElement | null } {
  const el = document.elementFromPoint(x, y) as HTMLElement | null;
  const g = el?.closest('[data-group]') as HTMLElement | null;
  const name = g?.dataset.group ?? null;
  return { group: name && name !== '' ? name : null, el: g };
}

function scrollHost(): HTMLElement | null {
  return (document.querySelector('.sidelist') as HTMLElement | null) ?? null;
}

export function useHostDrag(onMove: (hostId: string, group: string) => void): HostDragApi {
  // 整个拖拽过程不触发 React 重渲染：ghost / 高亮都直接操作 DOM（低配机不掉帧）
  const ref = useRef<Active | null>(null);
  const suppressUntil = useRef(0);
  const onMoveRef = useRef(onMove);
  onMoveRef.current = onMove;
  const handlersRef = useRef<Handlers | null>(null);
  const finishRef = useRef<(drop: boolean) => void>(() => {});

  /**
   * 只在首次渲染时创建一组监听器（稳定引用）：
   * 它们只读写 ref 与 DOM，不依赖任何渲染期状态，因此可以安全复用。
   */
  const handlers = (handlersRef.current ??= {
    move: (ev: PointerEvent) => {
      const a = ref.current;
      if (!a) return;
      // 窗口外松手且窗口未失焦时，之后的 pointerup 不属于本拖拽——
      // 校验 pointerId 与 buttons，防止“残留拖拽态 + 下一次普通点击误移分组”。
      if (ev.pointerId !== a.pointerId) return;
      if (ev.buttons === 0) {
        // 指针已无按键（漏掉了 pointerup/pointercancel）：按取消收尾
        finishRef.current(false);
        return;
      }
      if (!a.started) {
        if (Math.hypot(ev.clientX - a.x, ev.clientY - a.y) < THRESHOLD_PX) return;
        a.started = true;
        // pointer capture 放在「真正开始拖」之后，而不是 pointerdown 时。
        // 提前捕获会让 WebView2 把同一指针序列的 click 重定向到捕获元素（整行），
        // 行内 ▶ 这类自绘按钮的 onClick 就永远收不到——表现为「点 ▶ 不连接、
        // 只有双击（dblclick 命中的是行本身）才行」。
        try { a.captureEl?.setPointerCapture?.(a.pointerId); } catch { /* 已释放/不支持 */ }
        document.body.classList.add('host-dragging');
        document.getSelection()?.removeAllRanges();
        const ghost = document.createElement('div');
        ghost.className = 'hostdrag-ghost';
        ghost.dataset.dragHost = a.hostId;
        const nameEl = document.createElement('span');
        nameEl.className = 'hd-name';
        nameEl.textContent = a.hostName;
        const hintEl = document.createElement('span');
        hintEl.className = 'hd-hint';
        ghost.append(nameEl, hintEl);
        document.body.appendChild(ghost);
        a.ghost = ghost;
        a.srcEl = document.querySelector(`.hostitem[data-arg="${CSS.escape(a.hostId)}"]`);
        a.srcEl?.setAttribute('data-dragsrc', '1');
      }
      ev.preventDefault();
      paintTarget(a, ev.clientX, ev.clientY);
      updateEdge(a);
    },
    up: (ev: PointerEvent) => {
      const a = ref.current;
      if (!a) return;
      // 只响应本拖拽的 pointerup（多指/残留监听不误触发）
      if (ev.pointerId !== a.pointerId) return;
      if (a.started) {
        ev.preventDefault();
        paintTarget(a, ev.clientX, ev.clientY);   // 松手处再算一次，防止最后一次 move 丢事件
      }
      finishRef.current(true);
    },
    cancel: () => finishRef.current(false),
    key: (ev: KeyboardEvent) => {
      if (ev.key !== 'Escape') return;
      if (!ref.current) return;   // 没有拖拽时绝不吃 Esc（防监听器泄漏时产生全局副作用）
      ev.preventDefault();
      ev.stopPropagation();
      finishRef.current(false);
    },
    blur: () => finishRef.current(false),
  });

  const paintGhost = (a: Active, x: number, y: number) => {
    const g = a.ghost;
    if (!g) return;
    g.style.transform = `translate3d(${x + 12}px, ${y + 10}px, 0)`;
    const hint = g.querySelector('.hd-hint') as HTMLElement | null;
    if (hint) hint.textContent = a.over && a.over !== a.from ? `→ ${a.over}` : '';
  };

  /** 命中 + 高亮 + ghost 跟随。落点必须是「别的分组」；同组视为无效落点。 */
  const paintTarget = (a: Active, x: number, y: number) => {
    a.lastX = x;
    a.lastY = y;
    const hit = hitGroup(x, y);
    const next = hit.group && hit.group !== a.from ? hit.group : null;
    if (next !== a.over) {
      a.hlEl?.removeAttribute('data-drop');
      a.hlEl = null;
      a.over = next;
    }
    if (next) {
      // 用 data-drop 属性标记：React reconcile 只重写 className / 已知 props，
      // 不会清掉未知属性，因此拖拽途中列表重渲染也不会丢高亮。
      const header = (a.hlEl?.isConnected ? a.hlEl : document.querySelector(`.grouprow[data-group="${CSS.escape(next)}"]`)) as HTMLElement | null;
      if (header) {
        if (header.getAttribute('data-drop') !== '1') header.setAttribute('data-drop', '1');
        a.hlEl = header;
      }
    }
    paintGhost(a, x, y);
  };

  const stopAutoScroll = (a: Active | null) => {
    if (!a) return;
    if (a.raf !== null) { cancelAnimationFrame(a.raf); a.raf = null; }
    a.scrollDir = 0;
  };

  const tickAutoScroll = () => {
    const a = ref.current;
    if (!a || !a.started) return;
    const list = scrollHost();
    if (!list) return;
    if (a.scrollDir === 0) { a.raf = null; return; }
    list.scrollTop += a.scrollDir * SCROLL_STEP;
    paintTarget(a, a.lastX, a.lastY);   // 滚动后落点可能变化：用最后坐标重新命中
    a.raf = requestAnimationFrame(tickAutoScroll);
  };

  const updateEdge = (a: Active) => {
    const list = scrollHost();
    if (!list) { a.scrollDir = 0; return; }
    const r = list.getBoundingClientRect();
    const dir: -1 | 0 | 1 = a.lastY < r.top + EDGE_PX ? -1 : a.lastY > r.bottom - EDGE_PX ? 1 : 0;
    if (dir === a.scrollDir) return;
    a.scrollDir = dir;
    if (dir !== 0 && a.raf === null) a.raf = requestAnimationFrame(tickAutoScroll);
  };

  finishRef.current = (drop: boolean) => {
    const a = ref.current;
    if (!a) return;
    // 顺序很重要：先清 DOM / 监听器，最后才清 ref（反过来清理函数读到 null 会直接 return，
    // ghost 与高亮就永久留在界面上）。
    stopAutoScroll(a);
    cleanupDom(a);
    try { if (a.captureEl?.hasPointerCapture?.(a.pointerId)) a.captureEl.releasePointerCapture(a.pointerId); } catch { /* 已释放 */ }
    window.removeEventListener('pointermove', handlers.move);
    window.removeEventListener('pointerup', handlers.up);
    window.removeEventListener('pointercancel', handlers.cancel);
    window.removeEventListener('keydown', handlers.key, true);
    window.removeEventListener('blur', handlers.blur);
    ref.current = null;
    if (!a.started) return;
    suppressUntil.current = Date.now() + 400;   // 吞掉拖拽尾随的 click
    if (drop && a.over && a.over !== a.from) onMoveRef.current(a.hostId, a.over);
  };

  const cancel = useCallback(() => { finishRef.current(false); }, []);

  // 组件卸载时清理，避免 ghost / 监听器残留
  useEffect(() => () => { finishRef.current(false); }, []);

  const onPointerDown = useCallback((ev: ReactPointerEvent, host: { id: string; name: string; group: string }) => {
    if (ev.button !== 0) return;
    // 行内的连接按钮 / 链接 / 输入框不作为拖拽起点（否则点 ▶ 也开始拖）
    const t = ev.target as HTMLElement | null;
    if (t?.closest(NO_DRAG_SEL)) return;
    if (ref.current) return;
    cleanupDom(null);   // 防御：上一次异常（热重载等）残留的 ghost / 高亮先清掉
    ref.current = {
      hostId: host.id, hostName: host.name, from: host.group,
      x: ev.clientX, y: ev.clientY, pointerId: ev.pointerId,
      started: false, over: null, ghost: null, srcEl: null,
      captureEl: ev.currentTarget as HTMLElement,
      hlEl: null, scrollDir: 0, raf: null, lastX: ev.clientX, lastY: ev.clientY,
    };
    // 指针捕获在拖拽真正开始时才设置（见 handlers.move），
    // 保证拖出窗口松手仍能收到 pointerup/pointercancel，同时不劫持普通点击。
    window.addEventListener('pointermove', handlers.move);
    window.addEventListener('pointerup', handlers.up);
    window.addEventListener('pointercancel', handlers.cancel);
    window.addEventListener('keydown', handlers.key, true);
    window.addEventListener('blur', handlers.blur);
  }, [handlers]);

  const consumeClickSuppress = useCallback(() => {
    if (Date.now() < suppressUntil.current) { suppressUntil.current = 0; return true; }
    return false;
  }, []);

  return { onPointerDown, consumeClickSuppress, cancel };
}
