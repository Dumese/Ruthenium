import { useEffect } from 'react';

/** Sheet/Modal 打开时焦点进入并在内部循环，关闭后焦点回到触发元素 */
export function useFocusTrap(active: boolean) {
  useEffect(() => {
    if (!active) return;
    const prev = document.activeElement as HTMLElement | null;
    // 可能同时存在底层 Sheet + 上层配对/控制确认框：焦点圈定最后一个（最上层）。
    const nodes = document.querySelectorAll<HTMLElement>('.sheet, .modal');
    const root = nodes.length ? nodes[nodes.length - 1] : null;
    if (root) {
      const focusable = root.querySelector<HTMLElement>('input, textarea, select, button');
      focusable?.focus();
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== 'Tab' || !root) return;
      const els = Array.from(root.querySelectorAll<HTMLElement>('input, textarea, select, button'))
        .filter((el) => !el.hasAttribute('disabled'));
      if (!els.length) return;
      const first = els[0], last = els[els.length - 1];
      if (e.shiftKey && document.activeElement === first) { e.preventDefault(); last.focus(); }
      else if (!e.shiftKey && document.activeElement === last) { e.preventDefault(); first.focus(); }
    };
    document.addEventListener('keydown', onKey);
    return () => {
      document.removeEventListener('keydown', onKey);
      prev?.focus?.();
    };
  }, [active]);
}
