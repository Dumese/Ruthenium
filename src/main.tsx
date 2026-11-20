// 应用入口：挂载 <App/> 到 index.html 的 #root。
// 全局样式按 tokens → base → components/terminal 的顺序由组件按需引入；
// 这里只引入字体声明，保证首帧就有正确字形，避免终端/界面字体闪烁。
// 字体声明必须最先求值：useXterm 在模块顶层就会预加载等宽字体，
// 若 @font-face 尚未注册，预加载会静默失败并让终端用回退字体量字符尺寸。
import './styles/fonts.css';
import { createRoot } from 'react-dom/client';
import App from './App';
import { StrictMode } from 'react';
import { uiReady } from './ipc/commands';
import { preloadTermEngine } from './components/terminal/useXterm';
import { HelloGate } from './components/security/HelloGate';
import { markAppShown } from './lib/appReady';
import { isLang, setLang } from './i18n';

// 门禁在 App/load_config 之前渲染；用上次选择的语言初始化，避免英文/日文用户
// 在 Hello 锁定页先看到中文。配置里的 lang 稍后仍会覆盖。
try {
  const saved = localStorage.getItem('ruthenium.lang');
  if (isLang(saved)) setLang(saved);
} catch { /* localStorage 不可用 */ }

createRoot(document.getElementById('root')!).render(
  <StrictMode>
    {/* 启动门禁（设置里开启 Windows Hello 时）：验证通过才挂载 App，
        未验证时配置与会话都不会被加载（见 HelloGate 顶部注释）。 */}
    <HelloGate>
      <App />
    </HelloGate>
  </StrictMode>,
);

// 窗口显示时机：等**字体就绪**再通知后端显示。
// 原因：界面字体是内嵌的全量 Pan-CJK Noto Sans CJK（约 13MB），用 font-display:block
// 时字体解析完成前文字不可见；如果此时就把窗口露出来，用户会看到「空白窗口」再突然出字。
// 这里把窗口显示推迟到「字体就绪 + 至少绘制过一帧」，代价是冷启动多等字体解析（约 0.2–0.4s），
// 换来的是窗口一出现就是完整界面。1.2s 上限保证字体异常时不会一直不显示（后端另有 1.5s 兜底）。
const fontsReady = (): Promise<unknown> =>
  Promise.race([
    document.fonts?.ready ?? Promise.resolve(),
    new Promise((resolve) => setTimeout(resolve, 1200)),
  ]);
void fontsReady().then(() => {
  requestAnimationFrame(() => {
    requestAnimationFrame(() => {
      // 窗口显示完成（或失败）后通知 HelloGate：系统验证框要弹在可见窗口之上
      uiReady().catch(() => { /* no-op */ }).finally(markAppShown);
    });
  });
});

// 窗口显示后**空闲**预热终端引擎（xterm 模块 + 等宽字体）：第一次 Ctrl+T / 连接不必现等动态 import；
// 放在窗口显示之后，低配机上不与首屏抢 CPU。
{
  const idle = (window as unknown as {
    requestIdleCallback?: (cb: () => void, o?: { timeout: number }) => number;
  }).requestIdleCallback;
  if (idle) idle(() => preloadTermEngine(), { timeout: 4000 });
  else setTimeout(() => preloadTermEngine(), 4000);
}
