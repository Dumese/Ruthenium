// xterm 实例管理：模块级 Map<sessionId, instance>，绝不进 store
// term.onData → write_terminal；term://data → base64 → Uint8Array → term.write；
// ResizeObserver 防抖 80ms → resize_terminal。
// 每个实例都挂 SearchAddon（Ctrl+F 查找）与 WebLinksAddon（URL 点击）。
// 类型只在编译期使用；运行时代码按需动态加载（见 loadXterm）。
// 这样 xterm 及其三个 addon（约 500KB）不会进入首屏主包，启动只需解析应用外壳。
import type { Terminal } from '@xterm/xterm';
import type { FitAddon } from '@xterm/addon-fit';
import type { WebglAddon } from '@xterm/addon-webgl';
import type { SearchAddon, ISearchDecorationOptions } from '@xterm/addon-search';
import type { WebLinksAddon } from '@xterm/addon-web-links';

/** 运行时按需加载的 xterm 模块集合（加载一次后复用同一个 Promise）。 */
interface XtermMods {
  Terminal: typeof import('@xterm/xterm').Terminal;
  FitAddon: typeof import('@xterm/addon-fit').FitAddon;
  WebglAddon: typeof import('@xterm/addon-webgl').WebglAddon;
  SearchAddon: typeof import('@xterm/addon-search').SearchAddon;
  WebLinksAddon: typeof import('@xterm/addon-web-links').WebLinksAddon;
}
let xtermModsPromise: Promise<XtermMods> | null = null;

/** 首次打开终端时加载 xterm（只加载一次）。 */
function loadXterm(): Promise<XtermMods> {
  xtermModsPromise ??= Promise.all([
    import('@xterm/xterm'),
    import('@xterm/addon-fit'),
    import('@xterm/addon-search'),
    import('@xterm/addon-web-links'),
    import('@xterm/addon-webgl'),
  ]).then(([core, fit, search, links, webgl]) => ({
    Terminal: core.Terminal,
    FitAddon: fit.FitAddon,
    SearchAddon: search.SearchAddon,
    WebLinksAddon: links.WebLinksAddon,
    WebglAddon: webgl.WebglAddon,
  }));
  return xtermModsPromise;
}
import { writeTerminal, resizeTerminal } from '../../ipc/commands';
import { useAppStore } from '../../store/useAppStore';
import { termStack } from '../../lib/termfonts';
import { gpuAccelerationEnabled, gpuAvailable } from '../../lib/gpu';
import { openExternal } from '../../lib/openExternal';
import type { GpuMode } from '../../types/models';

interface TermInstance {
  term: Terminal;
  fit: FitAddon;
  /** 终端内查找：每个实例一个，Esc / 切会话时 clearDecorations */
  search: SearchAddon;
  /** URL 点击：只处理 http(s)，交给系统浏览器 */
  links: WebLinksAddon | null;
  ro: ResizeObserver | null;
  timer: ReturnType<typeof setTimeout> | null;
  /** 当前 sessionId：reconnect 会换 id，onData / resize 必须跟随最新值 */
  sessionId: string;
  /** 已加载的 WebGL 渲染器；gpuMode=off 时置 null 并回退 DOM */
  webgl: WebglAddon | null;
  /** 最近一次已同步给后端的 PTY 尺寸：尺寸没变时不再发 resize_terminal（省 IPC） */
  lastCols: number;
  lastRows: number;
  /** 最近一次 fit 时的容器尺寸：切标签重新挂载时尺寸没变就跳过 fit（省一次布局 + xterm resize） */
  lastFitW: number;
  lastFitH: number;
}

const terms = new Map<string, TermInstance>();

/** 按设备能力选择回滚行数：低配 1500，正常 4000。 */
function scrollbackForDevice(): number {
  const nav = navigator as Navigator & { deviceMemory?: number };
  const mem = nav.deviceMemory; // Chromium 提供的粗粒度内存（GB）
  const cores = nav.hardwareConcurrency || 8;
  return (mem !== undefined && mem <= 4) || cores <= 4 ? 1500 : 4000;
}

/** 空闲预热终端引擎（xterm 模块 + 等宽字体）：第一次 Ctrl+T / 连接不必现等动态 import。 */
export function preloadTermEngine(): void {
  void loadXterm().catch(() => { /* ignore */ });
  void ensureTermFonts(useAppStore.getState().fontSize).catch(() => { /* ignore */ });
}

/** 三款内嵌等宽字体：终端创建前必须就绪，否则 xterm 会用回退字体量字符尺寸，
 *  导致「字形是等宽、网格按比例字体」的错位（表现为文字偏小、行距偏大）。 */
const MONO_FACES = ['"Rut Mono JB"', '"Rut Mono Plex"', '"Rut Mono SC"'];

// 不要在启动窗口预热等宽字体：那会在首帧后 36–56 ms 抢 Tauri 资产协议带宽
// （与 UI 字体争用），且没有收益。
// 首次创建终端时 `ensureTermFonts()` 会 await 它们，功能不受影响。

/**
 * 应用字号/字体/光标/**行高**。
 *
 * xterm 的单元格高度 = floor(字体自然行高 × lineHeight)，而界面的「行高」是**字号倍数**
 * （默认 1.3 → 行距 = 字号 × 1.3）。这里按自然行高换算成 xterm 的系数，并在字体迟到时
 * 清掉 WebGL 字形图集，避免沿用回退字体的旧网格。
 */
/** 等三款等宽字体全部就绪（首次挂载会等到字体可用，避免用回退字体量字符尺寸）。 */
async function ensureTermFonts(fontSize: number): Promise<void> {
  if (typeof document === "undefined" || !document.fonts) return;
  await Promise.all(
    MONO_FACES.map((f) => document.fonts.load(`${fontSize}px ${f}`).catch(() => undefined)),
  );
  await document.fonts.ready;
}

/** 当前单元格高度（调试用）：优先「画布高度 ÷ 行数」。 */
function cellHeightOf(inst: TermInstance): number {
  const canvas = inst.term.element?.querySelector('.xterm-screen canvas') as HTMLCanvasElement | null;
  const rows = inst.term.rows;
  if (canvas && rows > 0) {
    const h = canvas.getBoundingClientRect().height / rows;
    if (h > 0) return h;
  }
  return 0;
}

/** 字体自然行高（px）：用 canvas 的 fontBoundingBox 度量，与 xterm 的量测口径最接近。 */
function naturalLineHeight(fontStack: string, fontSize: number): number {
  try {
    const ctx = document.createElement('canvas').getContext('2d');
    if (ctx) {
      ctx.font = `${fontSize}px ${fontStack}`;
      const m = ctx.measureText('Mg');
      const h = (m.fontBoundingBoxAscent ?? 0) + (m.fontBoundingBoxDescent ?? 0);
      if (h > 0) return h;
    }
  } catch { /* 老内核没有 fontBoundingBox：走下面的经验值 */ }
  return fontSize * 1.31;
}

function applyTermMetrics(inst: TermInstance, st: { fontSize: number; font: string; cursor: string; lineHeight: number }): void {
  const { term } = inst;
  const stack = termStack(st.font);
  // fontFamily 设为相同值不会触发字符量测；先切到一个临时值再切回，
  // 保证字体迟到（Web 字体刚加载完）时网格按真实字形重建。
  if (term.options.fontFamily === stack) {
    term.options.fontFamily = `${stack},monospace`;
  }
  term.options.fontFamily = stack;
  term.options.fontSize = st.fontSize;
  term.options.cursorStyle = st.cursor === 'bar' ? 'bar' : 'block';
  // xterm 的 cell.height = 字体自然行高 × lineHeight；的「行高」是字号倍数，
  // 因此换算系数 = (字号 × 行高倍数) / 字体自然行高。
  // 注意：xterm 校验 lineHeight ≥ 1，小于 1 会直接抛异常（导致整树卸载白屏），
  // 所以这里必须夹到 [1, 3]。用户可调范围是 1.0–2.0，但**实际下限受字体自然行高限制**
  // （等宽字体约 1.2–1.35 倍字号）：再往下 xterm 无法渲染，[1.0, 自然行高] 区间都落在下限。
  try {
    const target = st.fontSize * (st.lineHeight || 1.3);
    const natural = naturalLineHeight(stack, st.fontSize);
    const ratio = natural > 0 ? target / natural : 1.4;
    term.options.lineHeight = Math.min(3, Math.max(1, ratio));
  } catch (e) {
    console.warn('[term] 行高应用失败，保持当前值：', e);
  }
  try { inst.webgl?.clearTextureAtlas(); } catch { /* 图集重建失败不影响 DOM 回退 */ }
  if (typeof window !== 'undefined' && (window as unknown as { __RUTHENIUM_E2E__?: boolean }).__RUTHENIUM_E2E__) {
    (window as unknown as Record<string, unknown>).__termDebug = () => ({ marker: 'v3',
      cellH: cellHeightOf(inst),
      taStyle: (term.element?.querySelector('.xterm-helper-textarea') as HTMLElement | null)?.getAttribute('style') ?? null,
      fontCheck: typeof document !== 'undefined' && document.fonts ? document.fonts.check(`13px "Rut Mono JB"`) : null,
      canvasW: (() => {
        try {
          const ctx = document.createElement('canvas').getContext('2d');
          if (!ctx) return null;
          ctx.font = '13px "Rut Mono JB"';
          return ctx.measureText('M').width;
        } catch { return null; }
      })(),
      font: term.options.fontFamily,
      fontSize: term.options.fontSize,
      lineHeight: term.options.lineHeight,
      rows: term.rows,
      cols: term.cols,
    });
  }
}

/** 行号等渲染型订阅：终端内容/滚动/尺寸变化时通知（rAF 合并） */
const renderSubs = new Map<string, Set<() => void>>();
/** 本帧内待通知的会话集合：同帧内多个会话都要各自收到通知
 *  （单 rAF 句柄直接 return 会让第二个会话的 gutter 漏掉本帧更新）。 */
const pendingNotify = new Set<string>();
let renderRaf = 0;
function notifyRender(sessionId: string): void {
  pendingNotify.add(sessionId);
  if (renderRaf) return;
  renderRaf = requestAnimationFrame(() => {
    renderRaf = 0;
    const ids = [...pendingNotify];
    pendingNotify.clear();
    for (const id of ids) {
      renderSubs.get(id)?.forEach((cb) => {
        try { cb(); } catch { /* ignore */ }
      });
    }
  });
}

/** 连接握手期间后端可能已开始推送 term://data；xterm 尚未挂载时先缓冲，挂载后回放。
 *  上限 256 KiB/会话，超出丢弃最旧数据，防止异常情况吃内存。 */
const MAX_PENDING_BYTES = 256 * 1024;
/** 已挂载会话的写队列字节上限（rAF 停摆时的防 OOM），丢最旧。 */
const MAX_WRITE_QUEUE_BYTES = 4 * 1024 * 1024;
interface PendingChunks { chunks: Uint8Array[]; bytes: number }
const pendingData = new Map<string, PendingChunks>();

// 应用级组合键（由 window capture 的 useHotkeys 处理）：xterm 不再消费，避免
// 同一个按键既触发 UI 又向远程 shell 写入控制字符。Ctrl+W 必须留给 shell 删词。
function appOwnedChord(e: KeyboardEvent): boolean {
  if (!e.ctrlKey || e.altKey || e.metaKey) return false;
  const k = e.key.toLowerCase();
  if (k === 'w') return e.shiftKey;
  if (k === 'tab') return true;
  if (k === 'k' || k === 't' || k === 'n' || k === 'b' || k === ',' || k === 'f' || k === 'l') return true;
  return /^[1-9]$/.test(k);
}

function themeFromCss(): Parameters<Terminal['open']>[0] extends never ? never : Record<string, string> {
  const cs = getComputedStyle(document.body);
  const v = (name: string) => cs.getPropertyValue(name).trim();
  return {
    background: v('--term') || '#fff',
    foreground: v('--term-t') || '#26262b',
    cursor: v('--accent') || '#0071e3',
    selectionBackground: v('--accent2') || 'rgba(0,113,227,.10)',
    // 滚动条滑块：默认更淡，悬停/拖动时才明显（配合 terminal.css 的 6px 宽度）
    scrollbarSliderBackground: 'rgba(128,128,132,.28)',
    scrollbarSliderHoverBackground: 'rgba(128,128,132,.48)',
    scrollbarSliderActiveBackground: 'rgba(128,128,132,.60)',
  };
}

/**
 * 轻量性能计数器（供 e2e / 开发测量）：refit / resize / 挂载次数。
 * 每项只是一次自增，生产开销可忽略；通过 __termPerf() 读取。
 */
const perfCounters = { fit: 0, resize: 0, mount: 0 };

/** 立即按容器尺寸重排一次并同步后端 PTY 尺寸（不走防抖）。 */
/**
 * 立即按容器尺寸重排一次并同步后端 PTY 尺寸（不走防抖）。
 *
 * force=false 时若容器宽高与上次 fit 相同则直接返回：切标签会把同一个 xterm DOM
 * 挪回容器，尺寸没变，FitAddon.fit() + xterm resize 纯属浪费（低配机上这是标签
 * 切换卡顿的主要来源）。字号 / 行高 / 字体变化必须 force=true。
 */
function refitNow(t: TermInstance, force = false): void {
  try {
    if (!force) {
      const host = t.term.element?.parentElement;
      if (host) {
        const w = host.clientWidth;
        const h = host.clientHeight;
        if (w === t.lastFitW && h === t.lastFitH) return;
        t.lastFitW = w;
        t.lastFitH = h;
      }
    }
    perfCounters.fit += 1;
    t.fit.fit();
    // 兜底：DPR 变化 / 像素取整后仍可能多出不到一行的溢出，
    // 这里按量测结果再收一行，保证最后一行永远不被底部裁切。
    shrinkToBox(t);
    const { cols, rows } = t.term;
    // 尺寸没变就不发 IPC：窗口/全屏切换会补三拍 refit，N 个标签 × 3 次
    // 全是重复的 resize_terminal（后端只是个 window_change 请求）。
    if (cols !== t.lastCols || rows !== t.lastRows) {
      t.lastCols = cols;
      t.lastRows = rows;
      resizeTerminal(t.sessionId, cols, rows).catch(() => { /* 会话已关闭 / 不可见：忽略 */ });
    }
  } catch { /* 容器不可见 / 尚未 open：等下一次尺寸事件 */ }
}

/**
 * 网格若超出 `.xterm` 内容盒，按量测到的单元格尺寸收紧行列（只减不增）。
 * 正常路径 FitAddon 已经算对（留白在 `.xterm` 自身），这里是保险：
 * 亚像素、DPR 切换、字体度量延迟时可能多算一行/一列，表现为底部或右侧被裁。
 */
function shrinkToBox(t: TermInstance): void {
  const el = t.term.element;
  const screen = el?.querySelector('.xterm-screen') as HTMLElement | null;
  if (!el || !screen || t.term.rows < 1 || t.term.cols < 1) return;
  const cs = getComputedStyle(el);
  const padV = (parseFloat(cs.paddingTop) || 0) + (parseFloat(cs.paddingBottom) || 0);
  const padH = (parseFloat(cs.paddingLeft) || 0) + (parseFloat(cs.paddingRight) || 0);
  const availH = el.clientHeight - padV;
  const availW = el.clientWidth - padH;
  if (availH <= 0 || availW <= 0) return;
  const rect = screen.getBoundingClientRect();
  const cellH = rect.height / t.term.rows;
  const cellW = rect.width / t.term.cols;
  if (!(cellH > 0) || !(cellW > 0)) return;
  // 0.5px 容差：亚像素舍入不算溢出，避免反复 resize 抖动。
  const rows = Math.max(1, Math.min(t.term.rows, Math.floor((availH + 0.5) / cellH)));
  const cols = Math.max(1, Math.min(t.term.cols, Math.floor((availW + 0.5) / cellW)));
  if (rows !== t.term.rows || cols !== t.term.cols) t.term.resize(cols, rows);
}

function fitOf(t: TermInstance): void {
  if (t.timer) clearTimeout(t.timer);
  t.timer = setTimeout(() => refitNow(t), 80);
}

/**
 * 窗口尺寸 / 全屏切换的补拟合。
 * ResizeObserver 在「窗口 ↔ 全屏 / 最大化」过渡、跨显示器 DPR 变化等场景下，
 * 可能不触发、或触发在布局稳定之前（WebView2 视口同步有延迟）。这里在
 * resize / fullscreenchange 之后再补三拍（rAF + 150ms + 400ms），
 * 保证行列按最终尺寸计算，最后一行不被裁。
 */
let viewportRefitInstalled = false;
function installViewportRefit(): void {
  if (viewportRefitInstalled || typeof window === 'undefined') return;
  viewportRefitInstalled = true;
  const passes = (): void => {
    requestAnimationFrame(() => terms.forEach((t) => refitNow(t)));
    setTimeout(() => terms.forEach((t) => refitNow(t)), 150);
    setTimeout(() => terms.forEach((t) => refitNow(t)), 400);
  };
  window.addEventListener('resize', passes, { passive: true });
  window.visualViewport?.addEventListener('resize', passes, { passive: true });
  document.addEventListener('fullscreenchange', passes);
  document.addEventListener('webkitfullscreenchange' as never, passes as never);
}

/* ---------- 查找高亮配色（从 --accent / --accent2 取） ---------- */
type Rgba = [number, number, number, number];

function cssVar(name: string): string {
  try { return getComputedStyle(document.body).getPropertyValue(name).trim(); } catch { return ''; }
}
function parseCssColor(c: string): Rgba | null {
  const s = String(c || '').trim();
  let m = /^#([0-9a-f]{3})$/i.exec(s);
  if (m) return [parseInt(m[1][0] + m[1][0], 16), parseInt(m[1][1] + m[1][1], 16), parseInt(m[1][2] + m[1][2], 16), 1];
  m = /^#([0-9a-f]{6})$/i.exec(s);
  if (m) return [parseInt(m[1].slice(0, 2), 16), parseInt(m[1].slice(2, 4), 16), parseInt(m[1].slice(4, 6), 16), 1];
  m = /^rgba?\(([^)]+)\)$/i.exec(s);
  if (m) {
    const p = m[1].split(/[,\s/]+/).filter(Boolean).map(Number);
    if (p.length >= 3 && p.slice(0, 3).every((n) => Number.isFinite(n))) return [p[0], p[1], p[2], Number.isFinite(p[3]) ? p[3] : 1];
  }
  return null;
}
function toHex(rgb: [number, number, number]): string {
  const h = (n: number) => Math.max(0, Math.min(255, Math.round(n))).toString(16).padStart(2, '0');
  return '#' + h(rgb[0]) + h(rgb[1]) + h(rgb[2]);
}
/** 把可能带透明度的颜色按 alpha 叠到终端底色上，得到 addon 要求的 #RRGGBB */
function blendOn(over: Rgba, base: [number, number, number]): string {
  const a = Math.max(0, Math.min(1, over[3]));
  return toHex([over[0] * a + base[0] * (1 - a), over[1] * a + base[1] * (1 - a), over[2] * a + base[2] * (1 - a)]);
}

/**
 * 查找装饰配色：全部颜色实时从 CSS 变量取（明暗主题各验一次）。
 * 普通匹配 = `--accent2` 叠到 `--term` 上；当前匹配 = `--accent` 叠一层更重的底，
 * 边线与 overview ruler 用 `--accent` 本色，保证浅色/深色下都可读。
 */
export function searchDecorations(): ISearchDecorationOptions {
  const accent = parseCssColor(cssVar('--accent')) ?? [0, 113, 227, 1];
  const accent2 = parseCssColor(cssVar('--accent2')) ?? [accent[0], accent[1], accent[2], 0.14];
  const termBgC = parseCssColor(cssVar('--term')) ?? [255, 255, 255, 1];
  const termBg: [number, number, number] = [termBgC[0], termBgC[1], termBgC[2]];
  const accentHex = toHex([accent[0], accent[1], accent[2]]);
  return {
    matchBackground: blendOn(accent2, termBg),
    matchBorder: accentHex,
    matchOverviewRuler: accentHex,
    activeMatchBackground: blendOn([accent[0], accent[1], accent[2], 0.38], termBg),
    activeMatchBorder: accentHex,
    activeMatchColorOverviewRuler: accentHex,
  };
}

/** 查找栏是否应该在终端上下文里生效（终端聚焦或焦点已在查找框内） */
export function isTermFocused(): boolean {
  const el = document.activeElement as HTMLElement | null;
  if (!el) return false;
  return !!el.closest?.('.xterm') || !!el.closest?.('.termfind') || el.classList?.contains('termfind');
}

export interface SearchResultInfo { resultIndex: number; resultCount: number }

/** 订阅查找结果变化（n/m 计数）；返回取消函数 */
export function onTermSearchResults(sessionId: string, cb: (r: SearchResultInfo) => void): () => void {
  const inst = terms.get(sessionId);
  if (!inst) return () => { /* 无实例 */ };
  const sub = inst.search.onDidChangeResults((e) => cb({ resultIndex: e.resultIndex, resultCount: e.resultCount }));
  return () => { try { sub.dispose(); } catch { /* ignore */ } };
}

export function searchNext(sessionId: string, term: string, incremental = false): boolean {
  const inst = terms.get(sessionId);
  if (!inst || !term) return false;
  try {
    return inst.search.findNext(term, { incremental, caseSensitive: false, regex: false, decorations: searchDecorations() });
  } catch (e) {
    console.warn('[term] 查找失败：', e);
    return false;
  }
}

export function searchPrevious(sessionId: string, term: string): boolean {
  const inst = terms.get(sessionId);
  if (!inst || !term) return false;
  try {
    return inst.search.findPrevious(term, { caseSensitive: false, regex: false, decorations: searchDecorations() });
  } catch { return false; }
}

/** Esc / 关闭查找：清装饰 + 清选中（页面里不再残留高亮） */
export function clearTermSearch(sessionId: string): void {
  const inst = terms.get(sessionId);
  if (!inst) return;
  try { inst.search.clearDecorations(); } catch { /* ignore */ }
  try { inst.term.clearSelection(); } catch { /* ignore */ }
}

async function createTerm(sessionId: string, container: HTMLElement): Promise<TermInstance> {
  const { Terminal, FitAddon, SearchAddon, WebLinksAddon, WebglAddon } = await loadXterm();
  const st = useAppStore.getState();
  const term = new Terminal({
    fontSize: st.fontSize,
    fontFamily: termStack(st.font),
    cursorStyle: st.cursor === 'bar' ? 'bar' : 'block',
    // 先给一个保守值，open() 之后由 applyTermMetrics 按「字号 × 用户行高」精确校准
    lineHeight: 1.4,
    theme: themeFromCss() as never,
    // 低配机（≤4GB 内存或 ≤4 核）把回滚缓冲降到 1500：xterm 每次裁剪缓冲/
    // 重绘的代价与行数成正比，2GB 内存的机器上 4000 行 × 多标签也不划算。
    scrollback: scrollbackForDevice(),
    // @xterm/addon-search 的 decorations（高亮 + n/m 计数）走 xterm 的提案 API
    // （registerDecoration 在 xterm 6 仍标记为 proposed）：不开这个开关 addon 会抛错。
    allowProposedApi: true,
  });
  const fit = new FitAddon();
  term.loadAddon(fit);
  // 终端内查找（Ctrl+F），每个实例一个 addon
  const search = new SearchAddon();
  term.loadAddon(search);
  // URL 检测与点击 —— 不自己写正则、不内嵌浏览器，交给 addon 的默认 http(s) 匹配
  let links: WebLinksAddon | null = null;
  try {
    links = new WebLinksAddon((_event, uri) => { void openExternal(uri); });
    term.loadAddon(links);
  } catch (e) {
    links = null;
    console.warn('[term] WebLinksAddon 不可用：', e);
  }
  term.attachCustomKeyEventHandler((e) => !appOwnedChord(e));

  // OSC 7：shell 上报当前工作目录（file://host/path）。SFTP 面板默认打开这个目录，
  // 实现「FTP 跟随终端当前路径」。没有该序列时（多数默认 shell 不发）回落到服务器 home。
  try {
    term.parser.registerOscHandler(7, (data) => {
      try {
        const u = new URL(data);
        const p = decodeURIComponent(u.pathname);
        if (p && p.startsWith('/')) useAppStore.getState().setSessionCwd(inst.sessionId, p);
      } catch { /* 非法序列忽略 */ }
      return true;
    });
  } catch { /* 老内核不支持注册解析器时忽略 */ }
  term.open(container);
  const inst: TermInstance = {
    term, fit, search, links, ro: null, timer: null, sessionId, webgl: null,
    // 初始值取 "不可能" 的 0，保证首次 refit 一定会把真实尺寸同步给后端
    lastCols: 0, lastRows: 0, lastFitW: 0, lastFitH: 0,
  };
  // WebGL 渲染器：高刷新率 + 高分屏下明显更顺（DOM 渲染器每帧要重排大量节点）。
  // gpuMode=off 或本机探测不到 WebGL 时完全不加载，直接用 DOM 渲染器；
  // 失败（老 GPU / 禁用硬件加速 / 上下文丢失）自动回退 DOM，不影响功能。
  if (gpuAccelerationEnabled(st.gpuMode === 'off' ? 'off' : 'system') && gpuAvailable()) {
    try {
      const webgl = new WebglAddon();
      inst.webgl = webgl;
      webgl.onContextLoss(() => {
        if (inst.webgl === webgl) inst.webgl = null;
        try { webgl.dispose(); } catch { /* ignore */ }
      });
      term.loadAddon(webgl);
    } catch (e) {
      inst.webgl = null;
      console.warn('[term] WebGL 渲染不可用，回退 DOM 渲染：', e);
    }
  }
  // 字体/行高校准必须在 open() 之后（xterm 需要真实的量测容器）
  applyTermMetrics(inst, st);
  refitNow(inst, true);

  // open() 之后首帧才会写出单元格高度：等一帧再标定行高（此时量测才有效），
  // 然后重新 fit 一次，保证 rows/cols 与实际网格一致。
  requestAnimationFrame(() => {
    if (terms.get(sessionId) !== inst) return;
    applyTermMetrics(inst, useAppStore.getState());
    refitNow(inst, true);
    notifyRender(sessionId);
  });

  // 键盘输入 → write_terminal（真实远端 shell；Ctrl+L 清屏）
  // 用 inst.sessionId 而不是闭包参数：reconnect 换 id 后输入仍写到新会话。
  term.onData((data) => {
    if (data === '\u000c') { term.clear(); return; } // Ctrl+L
    // 会话刚关闭/尚未注册时 invoke 会 reject；按键是高频路径，必须吞掉，
    // 否则每次击键都产生 unhandled rejection（同 refitNow 里 resizeTerminal 的处理）。
    void writeTerminal(inst.sessionId, new TextEncoder().encode(data)).catch(() => { /* 会话已失效 */ });
  });

  // 尺寸同步：防抖 80ms
  if (typeof ResizeObserver !== 'undefined') {
    inst.ro = new ResizeObserver(() => fitOf(inst));
    inst.ro.observe(container);
  }

  // 行号 gutter 需要跟随内容/滚动变化（onRender 每帧触发，已用 rAF 合并）
  term.onRender(() => notifyRender(sessionId));
  term.onScroll(() => notifyRender(sessionId));
  term.onResize(() => { perfCounters.resize += 1; notifyRender(sessionId); });
  terms.set(sessionId, inst);
  // 连接过程中缓冲的首屏输出：实例已注册，按写预算队列回放。
  drainPending(sessionId);
  return inst;
}

/** 挂载点：attach/detach，按 sessionId 复用实例。
 *  detach 必须把终端 DOM 摘掉，否则同一容器里会累积多个 xterm 实例
 *  （表现为终端底部露出另一个实例的光标块）。 */
export function attachTerm(sessionId: string, container: HTMLElement): () => void {
  let disposed = false;
  const mount = (inst: TermInstance) => {
    perfCounters.mount += 1;
    const el = inst.term.element;
    if (el) {
      // 防御：容器里若残留其它终端的 DOM，先清掉，保证同时只挂一个
      for (const child of [...container.children]) {
        if (child !== el && (child as HTMLElement).classList?.contains('xterm')) {
          container.removeChild(child);
        }
      }
      if (el.parentElement !== container) {
        el.parentElement?.removeChild(el);
        container.appendChild(el);
      }
    }
    installViewportRefit();
    setTimeout(() => refitNow(inst), 0);
    inst.term.focus();
    inst.ro?.observe(container);
  };

  const existing = terms.get(sessionId);
  if (existing) {
    mount(existing);
  } else {
    // 首次挂载：先等字体就绪再建终端。否则 xterm 会用回退字体量出错误的字符宽度，
    // 字体随后加载后也不会重测（表现为字距过宽、行距偏大、列数偏少）。
    // 期间到达的数据由 pendingData 缓冲，不会丢。
    void Promise.all([ensureTermFonts(useAppStore.getState().fontSize), loadXterm()]).then(async () => {
      if (disposed || terms.has(sessionId)) return;
      const inst = await createTerm(sessionId, container);
      // 创建期间（动态 import / 字体等待）用户关掉了标签：disposeTerm 当时找不到实例
      // 直接返回了，这里必须补一次释放，否则 xterm/WebGL/监听会永久泄漏。
      if (disposed) {
        disposeTerm(sessionId);
        return;
      }
      mount(inst);
    });
  }

  return () => {
    // 只摘 DOM，保留实例与滚动缓冲；切回该标签时重新挂载
    disposed = true;
    const node = terms.get(sessionId)?.term.element;
    if (node && node.parentElement === container) container.removeChild(node);
    terms.get(sessionId)?.ro?.unobserve(container);
  };
}

export function disposeTerm(sessionId: string): void {
  pendingData.delete(sessionId);
  writeQueue.delete(sessionId);
  renderSubs.delete(sessionId);
  pendingNotify.delete(sessionId);
  const inst = terms.get(sessionId);
  if (!inst) return;
  inst.ro?.disconnect();
  if (inst.timer) clearTimeout(inst.timer);
  inst.webgl = null; // term.dispose() 会一并释放已加载的 addon
  inst.term.dispose();
  terms.delete(sessionId);
}

/**
 * 图形加速偏好即时生效：
 * - off：已打开的终端立刻 dispose WebGL 渲染器，回退 DOM 渲染；
 * - system：只影响之后新建的终端（已打开的不重建，重启后统一生效）。
 */
export function applyGpuMode(mode: GpuMode): void {
  if (mode !== 'off') return;
  terms.forEach((inst) => {
    const addon = inst.webgl;
    inst.webgl = null;
    if (!addon) return;
    try { addon.dispose(); } catch { /* 已损坏的上下文：忽略 */ }
    refitNow(inst, true);
  });
}

export function moveTerm(oldId: string, newId: string): void {
  const inst = terms.get(oldId);
  if (!inst) return;
  // 重连后新 PTY 可能用不同尺寸（如本地终端固定 120×32）：清掉 fit/行列缓存，
  // 否则挂载后的 refit 会因“容器尺寸/行列没变”提前返回，新 PTY 永远收不到 resize。
  inst.lastCols = 0;
  inst.lastRows = 0;
  inst.lastFitW = 0;
  inst.lastFitH = 0;
  inst.sessionId = newId;
  terms.delete(oldId);
  terms.set(newId, inst);
  // 后端在 reconnect 返回前就会推送新会话的首批 term://data（事件先于 invoke resolve）。
  // 这批数据按新 id 进了 pendingData，而本函数是唯一的重映射入口，必须在这里回放，
  // 否则重连后首屏/banner 永久丢失（createTerm 的回放只覆盖“实例从未创建”的场景）。
  pendingData.delete(oldId);
  drainPending(newId);
}

/** 片段插入：term.paste 只写入输入行，绝不执行 */
export function pasteToTerminal(sessionId: string, text: string): void {
  const inst = terms.get(sessionId);
  if (!inst) return;
  inst.term.paste(text);
  inst.term.focus();
}

/** 右键菜单「复制」：返回 xterm 里当前选中的文本（无选中返回空串） */
export function getTermSelection(sessionId: string): string {
  return terms.get(sessionId)?.term.getSelection() ?? '';
}

/** 右键菜单「复制」的可用性：无选中则该条目禁用 */
export function hasTermSelection(sessionId: string): boolean {
  return getTermSelection(sessionId).length > 0;
}

/** 右键菜单「全选」：选中整个缓冲区 */
export function selectAllTerm(sessionId: string): void {
  const inst = terms.get(sessionId);
  if (!inst) return;
  inst.term.selectAll();
  inst.term.focus();
}

/** 菜单动作执行后把焦点还给终端（Esc 收起菜单时不要丢掉输入焦点） */
export function focusTerm(sessionId: string): void {
  terms.get(sessionId)?.term.focus();
}

export function clearTerminal(sessionId: string): void {
  terms.get(sessionId)?.term.clear();
}

/** 应用主题/字号/字体变化到已存在实例 */
export function restyleTerms(): void {
  const st = useAppStore.getState();
  terms.forEach((inst) => {
    try {
      // 统一的度量入口：字号 / 字体 / 光标样式 / 行高（行高需按自然行高换算）
      applyTermMetrics(inst, st);
      inst.term.options.theme = themeFromCss() as never;
      refitNow(inst, true);
    } catch (e) {
      // 任何终端参数异常都不允许冒泡到 React（否则整棵树会被卸载）
      console.warn('[term] 重新应用外观失败：', e);
    }
  });
}

/**
 * term://data 事件入口。
 *
 * 高刷新率适配：SSH/PTY 的字节包可能远高于帧率（一次 ls 输出会拆成几十条 64KiB
 * 事件）。逐条 `term.write` 会让 xterm 每包解析一次并各自排一次重绘；这里按
 * requestAnimationFrame 合并成**每帧一次写入**，单帧写入量设上限，避免大输出
 * 帧内耗时过长导致掉帧。顺序仍然严格保持（同一会话的缓冲按到达顺序拼接）。
 */
const writeQueue = new Map<string, Uint8Array[]>();
let writeRaf = 0;

/** 单帧写入预算的自适应策略（纯函数，已单测）：见 writeBudget.ts。
 *  预算恒在 [MIN_BYTES_PER_FRAME, MAX_BYTES_PER_FRAME]，绝不允许收敛到 0——
 *  预算为 0 时既取不出字节、也不会再进入调整分支，写入队列会永久停滞。 */
import {
  MAX_BYTES_PER_FRAME,
  MIN_BYTES_PER_FRAME,
  nextFrameBudget,
} from './writeBudget';
let frameBudget = MAX_BYTES_PER_FRAME;

function flushWrites(): void {
  writeRaf = 0;
  for (const [sessionId, chunks] of writeQueue) {
    const inst = terms.get(sessionId);
    if (!inst || !chunks.length) {
      writeQueue.delete(sessionId);
      continue;
    }
    // 防御：预算若因异常路径落到非法区间（0 / NaN / 负数），先纠正回下限，
    // 否则本帧取不出任何字节、也进不了下面的调整分支，队列会永远停摆。
    if (!(frameBudget >= MIN_BYTES_PER_FRAME)) frameBudget = MIN_BYTES_PER_FRAME;
    // 按帧预算取用：超出部分留到下一帧，保持顺序。
    // 预算随量测到的解析耗时自适应：大输出时每帧最多堵 WRITE_BUDGET_MS，
    // 其余顺延到下一帧——总吞吐不变，但输入/滚动不会被一次 256KB 解析卡住。
    let budget = frameBudget;
    let taken = 0;
    let total = 0;
    for (const c of chunks) total += c.byteLength;
    const merged = new Uint8Array(Math.min(total, budget));
    let offset = 0;
    while (taken < chunks.length && budget > 0) {
      const chunk = chunks[taken];
      const n = Math.min(chunk.byteLength, budget);
      merged.set(chunk.subarray(0, n), offset);
      offset += n;
      budget -= n;
      if (n === chunk.byteLength) taken += 1;
      else chunks[taken] = chunk.subarray(n);
    }
    chunks.splice(0, taken);
    if (chunks.length) writeQueue.set(sessionId, chunks);
    else writeQueue.delete(sessionId);
    if (offset) {
      // 用 write 回调（解析完成）计时。write() 本身只是入队（微秒级），
      // 按入队耗时计时会让自适应预算恒收敛到 MAX，“低配机自动收紧”不会生效。
      const t0 = performance.now();
      const consumed = offset;
      inst.term.write(merged.subarray(0, offset), () => {
        frameBudget = nextFrameBudget(frameBudget, consumed, performance.now() - t0);
      });
    }
  }
  if (writeQueue.size && !writeRaf) writeRaf = requestAnimationFrame(flushWrites);
}

/**
 * 把某会话在 `pendingData` 中缓冲的首屏字节并入写队列。
 * 实例已存在（createTerm 完成 / reconnect 重映射）时调用；走同一套帧预算，避免一次写入卡顿。
 */
function drainPending(sessionId: string): void {
  const queued = pendingData.get(sessionId);
  if (!queued) return;
  pendingData.delete(sessionId);
  const q = writeQueue.get(sessionId) ?? [];
  q.push(...queued.chunks);
  writeQueue.set(sessionId, q);
  if (!writeRaf) writeRaf = requestAnimationFrame(flushWrites);
}

/**
 * base64 → Uint8Array（`term://data` 的线上格式）。
 *
 * 性能要点：先 `atob` 拿到一次性的二进制字符串，再用 for 循环写满预分配的
 * Uint8Array。不要用 `Uint8Array.from(atob(s), c => c.charCodeAt(0))`——那会先
 * 分配一个 JS 数组、再对每个字节调一次回调，64 KiB 负载要多花一个数量级的时间。
 * `atob` 在模块级缓存（首次用时取全局），避免每条事件都做一次全局查找。
 */
let atobFn: ((s: string) => string) | null = null;
function b64ToBytes(s: string): Uint8Array {
  const decode = (atobFn ??= atob);
  const bin = decode(s);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i += 1) out[i] = bin.charCodeAt(i);
  return out;
}

export function writeTermData(sessionId: string, data: string | Uint8Array): void {
  // 事件路径给 base64 字符串（后端 term://data）；编码提示等内部调用直接给字节。
  const chunk = typeof data === 'string' ? b64ToBytes(data) : data;
  const inst = terms.get(sessionId);
  if (!inst) {
    // 连接中标签还没挂 xterm：缓冲，挂载时回放（否则 cmd/SSH 首屏输出丢失）
    const q = pendingData.get(sessionId) ?? { chunks: [], bytes: 0 };
    q.chunks.push(chunk);
    q.bytes += chunk.byteLength;
    let dropped = false;
    while (q.bytes > MAX_PENDING_BYTES && q.chunks.length) {
      const old = q.chunks.shift();
      if (!old) break;
      q.bytes -= old.byteLength;
      dropped = true;
    }
    if (dropped) console.warn('[term] pending output exceeded 256 KiB, dropped oldest bytes for', sessionId);
    pendingData.set(sessionId, q);
    return;
  }
  const queue = writeQueue.get(sessionId) ?? [];
  queue.push(chunk);
  // 写队列字节上限（pendingData 有 256KiB 上限，这里也必须有）——
  // rAF 停摆（窗口最小化）+ 持续输出（tail -f / cat 大文件）会无界堆积直到 OOM。
  // 超限丢最旧并告警，策略与 pendingData 一致。
  let bytes = chunk.byteLength;
  for (const c of queue) bytes += c.byteLength;
  while (bytes > MAX_WRITE_QUEUE_BYTES && queue.length > 1) {
    const old = queue.shift();
    if (!old) break;
    bytes -= old.byteLength;
  }
  if (bytes > MAX_WRITE_QUEUE_BYTES) {
    console.warn('[term] write queue exceeded 4 MiB, dropped oldest bytes for', sessionId);
  }
  writeQueue.set(sessionId, queue);
  if (!writeRaf) writeRaf = requestAnimationFrame(flushWrites);
}

export function hasTerm(sessionId: string): boolean {
  return terms.has(sessionId);
}

/**
 * 测试专用：导出终端缓冲区文本。
 *
 * 启用 WebGL 渲染器后文本绘制在 canvas 上，`.xterm-rows` 的 DOM 文本为空，
 * 端到端测试无法再读 DOM 断言「回显是否正确」。只有测试在页面加载前注入
 * `window.__RUTHENIUM_E2E__ = true` 时才挂这个钩子，正式运行不暴露任何全局。
 */
// 只在开发与 E2E 构建（--mode e2e）里编译这段；生产构建中 MODE 为 production、DEV 为 false，
// 整个分支会被 esbuild 判定为死代码移除，正式包不含任何测试钩子字符串。
if ((import.meta.env.DEV || import.meta.env.MODE === 'e2e')
  && typeof window !== 'undefined'
  && (window as unknown as { __RUTHENIUM_E2E__?: boolean }).__RUTHENIUM_E2E__) {
  (window as unknown as Record<string, unknown>).__termText = (sessionId?: string): string => {
    const pick = (t: TermInstance) => {
      const buf = t.term.buffer.active;
      const lines: string[] = [];
      for (let i = 0; i < buf.length; i++) lines.push(buf.getLine(i)?.translateToString(true) ?? '');
      return lines.join('\n');
    };
    if (sessionId) {
      const one = terms.get(sessionId);
      return one ? pick(one) : '';
    }
    // 不指定会话时返回所有实例的文本（E2E 用它断言「某个终端里出现了这行」）
    return [...terms.values()].map(pick).join('\n');
  };
  (window as unknown as Record<string, unknown>).__termCount = (): number => terms.size;
  // 性能测量钩子：refit / resize / 挂载次数（tab 切换与尺寸变化开销定位用）
  (window as unknown as Record<string, unknown>).__termPerf = (): { fit: number; resize: number; mount: number } => ({ ...perfCounters });
  // 查找 / URL 点击的注册情况（无头浏览器据此断言每个终端都挂了 addon）
  (window as unknown as Record<string, unknown>).__termAddonInfo = (): { terms: number; search: number; links: number } => {
    let s = 0;
    let l = 0;
    terms.forEach((t) => { if (t.search) s += 1; if (t.links) l += 1; });
    return { terms: terms.size, search: s, links: l };
  };
  (window as unknown as Record<string, unknown>).__termSelection = (sessionId?: string): string => {
    if (sessionId) return terms.get(sessionId)?.term.getSelection() ?? '';
    return [...terms.values()].map((t) => t.term.getSelection()).join('');
  };
  // 明暗主题下的查找高亮配色：断言两套主题都从 --accent / --accent2 实时派生
  (window as unknown as Record<string, unknown>).__termSearchColors = (): ISearchDecorationOptions => searchDecorations();
}

/** 订阅终端渲染/滚动变化（返回取消函数） */
export function subscribeTermRender(sessionId: string, cb: () => void): () => void {
  const set = renderSubs.get(sessionId) ?? new Set<() => void>();
  set.add(cb);
  renderSubs.set(sessionId, set);
  return () => { set.delete(cb); };
}

/** 可见行快照：用于行号 gutter（不含正在输入的光标行） */
export function getTermViewport(
  sessionId: string,
): { numbers: (number | null)[]; cellH: number; padTop: number } | null {
  const inst = terms.get(sessionId);
  if (!inst) return null;
  const buf = inst.term.buffer.active;
  const rows = inst.term.rows;

  // 最后一个「有内容」的缓冲区行：行号只编到这里，
  // 避免把屏幕下方成片的空白行也编号（清屏后会出现 3、4、5… 一直排到底）。
  let lastContent = buf.viewportY - 1;
  for (let abs = buf.length - 1; abs >= buf.viewportY; abs--) {
    const line = buf.getLine(abs);
    if (line && line.translateToString(true).length > 0) { lastContent = abs; break; }
  }

  const numbers: (number | null)[] = [];
  for (let i = 0; i < rows; i++) {
    const abs = buf.viewportY + i;
    if (abs >= buf.length || abs > lastContent) { numbers.push(null); continue; }
    const line = buf.getLine(abs);
    // 编号规则（编辑器语义）：
    // - 缓冲区每一行都占号，包括空行与正在输入的提示符行，保证编号连续；
    // - 只编到「最后一个有内容的行」，屏幕下方成片空白不编号；
    // - 自动换行的续行不占新号（一个逻辑行只占一个号）。
    if (line?.isWrapped) { numbers.push(null); continue; }
    numbers.push(abs + 1);
  }

  // 几何：单元格高度与内容顶部偏移都从真实 DOM 取，
  // 这样字号/行高/字体变化后行号能立刻与新网格对齐（不能依赖缓存的 CSS 变量）。
  const screen = inst.term.element?.querySelector('.xterm-screen') as HTMLElement | null;
  // 基准取「外层 .termhost」（与行号栏同一个坐标原点）：.termhost 自身没有内边距，
  // .xterm 的 padding 由 screenRect 与 hostRect 的差值体现（= padding-top），
  // 所以行号首行仍与终端首行严格对齐。
  const host = inst.term.element?.parentElement as HTMLElement | null;
  const screenRect = screen?.getBoundingClientRect();
  const hostRect = host?.getBoundingClientRect();
  const cellH = screenRect && rows > 0 ? screenRect.height / rows : 0;
  const padTop = screenRect && hostRect ? screenRect.top - hostRect.top : 0;
  return { numbers, cellH, padTop };
}
