import { useCallback } from 'react';

/**
 * 侧栏拖拽（210–360px）。
 *
 * 高刷新率适配要点（120Hz 帧预算 8.3ms / 240Hz 4.17ms）：
 * 1. 指针事件用 requestAnimationFrame 合帧：一帧最多应用一次宽度，
 *    否则 240Hz 指针设备每秒会灌 240 次变更。
 * 2. 拖拽期间只写**侧栏元素自身**的 inline width，不写 documentElement 上的
 *    CSS 变量——自定义属性失效会波及整棵树，每次 mousemove 都会触发一次
 *    全树样式重算（240 次移动 = 154ms 样式重算）。
 * 3. 松手才提交到 store（触发持久化），拖拽过程零 React 渲染。
 */
export function useResizable(onCommit: (w: number) => void) {
  return useCallback(
    (e: React.MouseEvent) => {
      e.preventDefault();
      const startX = e.clientX;
      const target = document.querySelector<HTMLElement>('.sidebar');
      let latest = startX;
      let raf = 0;

      const clamp = (x: number) => Math.max(210, Math.min(360, x));

      const apply = () => {
        raf = 0;
        if (target) target.style.width = clamp(latest) + 'px';
      };

      const move = (ev: MouseEvent) => {
        latest = ev.clientX;
        if (!raf) raf = requestAnimationFrame(apply);
      };

      // 清理必须收敛到一处，并覆盖"拖到窗口外松手"的情况——那种情况
      // mouseup 不会派发到 document，mousemove 监听与 col-resize 光标会永久残留。
      const cleanup = () => {
        document.removeEventListener('mousemove', move);
        document.removeEventListener('mouseup', up);
        window.removeEventListener('blur', onBlur);
        document.body.style.cursor = '';
        if (raf) {
          cancelAnimationFrame(raf);
          raf = 0;
        }
      };
      const commit = (x: number) => {
        const w = clamp(x);
        // 落库前把最终宽度同时写回元素与变量，保证布局基准一致
        if (target) target.style.width = '';
        document.documentElement.style.setProperty('--sidew', w + 'px');
        onCommit(w);
      };
      function up(ev: MouseEvent) {
        cleanup();
        commit(ev.clientX);
      }
      function onBlur() {
        cleanup();
        commit(latest);
      }

      document.body.style.cursor = 'col-resize';
      document.addEventListener('mousemove', move);
      document.addEventListener('mouseup', up);
      window.addEventListener('blur', onBlur);
    },
    [onCommit],
  );
}
