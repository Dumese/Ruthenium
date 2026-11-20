// Windows 11「贴靠布局」接线：把最大化按钮的矩形同步给原生覆盖层，并把覆盖层
// 独占鼠标后丢失的 hover / press 观感补回来。
//
// 背景（详见 src-tauri/src/snap.rs 顶部注释）：
//   `decorations: false` 的窗口只有在自己回答 `WM_NCHITTEST = HTMAXBUTTON` 时，
//   系统才会给贴靠布局浮层；而 WebView2 是铺满客户区的子窗口，父窗口收不到那个点。
//   后端的做法是在按钮位置上盖一个原生子窗口回答 HTMAXBUTTON —— 代价是**那块矩形
//   的鼠标事件不再进 webview**：CSS `:hover` / `:active` / `onClick` 全部失效。
//
// 因此这里负责三件事：
//   1. 量按钮的 CSS 像素矩形（`getBoundingClientRect`）± 任何布局变化后重发；
//   2. 订阅后端转发的 `snap://hover`、`snap://press`，用类名复刻 hover/press；
//   3. 窗口最小化 / 专注模式（标题栏 display:none）/ 布局未就绪时发 0 矩形隐藏覆盖层。
//
// 为什么不做「右边缘 − 按钮宽 × n」的算术（不少插件这么干）：标题栏内边距、按钮宽度、
// 响应式断点任何一处改动都会让覆盖层错位到按钮外面，浮层就不在按钮上了。
import { useEffect, useState } from 'react';
import type { RefObject } from 'react';
import { isTauri } from '../../ipc/env';
import { snapOverlay } from '../../ipc/commands';
import { listen } from '../../ipc/events';
import { EV } from '../../types/events';

/** 覆盖层的最小有效边长：小于它一律当成「按钮不可见」（隐藏覆盖层）。 */
const MIN_SIDE = 2;

export interface SnapOverlayState {
  /** 指针在原生覆盖层上（等价于 `:hover`）。 */
  hover: boolean;
  /** 在覆盖层上按住左键（等价于 `:active`）。 */
  press: boolean;
}

/**
 * @param btnRef 最大化按钮的 ref（必须指向原生覆盖层要覆盖的那个元素）。
 * 非 Tauri 环境直接返回 `{hover:false, press:false}`，不注册任何监听。
 */
export function useSnapOverlay(btnRef: RefObject<HTMLElement | null>): SnapOverlayState {
  const [hover, setHover] = useState(false);
  const [press, setPress] = useState(false);

  useEffect(() => {
    if (!isTauri) return;
    const el = btnRef.current;
    if (!el) return;

    let disposed = false;
    let raf = 0;
    let mq: MediaQueryList | null = null;
    const subs: (() => void)[] = [];

    const push = () => {
      if (disposed || raf) return;
      raf = requestAnimationFrame(() => {
        raf = 0;
        if (disposed) return;
        const r = el.getBoundingClientRect();
        // 别直接用 r.width/r.height 当尺寸：按压反馈是 `transform: scale(.94)`，
        // 而 getBoundingClientRect 量的是**变换后**的矩形 —— 在按压态量一次，覆盖层
        // 就会比按钮小约 6%（46→43px），按钮左右各留一条缝。offsetWidth/offsetHeight
        // 是布局尺寸，不受 transform 影响；缩放以中心为原点，所以用中心反推左上角。
        const width = el.offsetWidth;
        const height = el.offsetHeight;
        // 专注模式会把整个标题栏 display:none → 尺寸全 0；窗口最小化时同理。
        const live = width >= MIN_SIDE && height >= MIN_SIDE && !document.hidden;
        const rect = live
          ? { x: r.left + (r.width - width) / 2, y: r.top + (r.height - height) / 2, width, height }
          : { x: 0, y: 0, width: 0, height: 0 };
        // 失败不能冒泡成未捕获异常（后端窗口尚未就绪、热重载等）：覆盖层只是增强。
        void snapOverlay(rect).catch(() => { /* 忽略：贴靠布局退化为没有浮层 */ });
      });
    };

    // 按钮自身尺寸/位置变化（含 hover 放大、字体就绪、标题栏换行）
    const ro = new ResizeObserver(push);
    ro.observe(el);
    ro.observe(document.documentElement);
    // 窗口缩放/最大化/还原：按钮尺寸不变但 x 会变，ResizeObserver 收不到
    window.addEventListener('resize', push);
    document.addEventListener('visibilitychange', push);

    // DPI 变化（拖到另一块缩放不同的显示器）：CSS 矩形不变，但后端要按新 DPI 重算
    // 物理像素，而且这个过程不一定触发 resize —— 就用 resolution 媒体查询盯着。
    const onDpr = () => { push(); watchDpr(); };
    const watchDpr = () => {
      mq?.removeEventListener('change', onDpr);
      mq = window.matchMedia(`(resolution: ${window.devicePixelRatio}dppx)`);
      mq.addEventListener('change', onDpr);
    };
    watchDpr();

    const subscribe = async (event: string, apply: (on: boolean) => void) => {
      try {
        const off = await listen<boolean>(event, apply);
        // StrictMode 的「挂载 → 卸载 → 再挂载」：卸载后才 resolve 的订阅要立刻退订
        if (disposed) off();
        else subs.push(off);
      } catch { /* 事件层不可用（浏览器 / mock）：只少了 hover 观感 */ }
    };
    void subscribe(EV.snapHover, setHover);
    void subscribe(EV.snapPress, setPress);

    push(); // 首帧：窗口此时可能还没 show，矩形已经算得出来

    return () => {
      disposed = true;
      if (raf) cancelAnimationFrame(raf);
      ro.disconnect();
      window.removeEventListener('resize', push);
      document.removeEventListener('visibilitychange', push);
      mq?.removeEventListener('change', onDpr);
      for (const off of subs) off();
      // 热重载/退出时别把覆盖层留在屏幕上
      void snapOverlay({ x: 0, y: 0, width: 0, height: 0 }).catch(() => {});
    };
  }, [btnRef]);

  return { hover, press };
}
