// 偏好 / 语言 / 视图设置 slice。
// 字段与 action 语义见 types.ts 的 AppStore 注释；本文件只组织实现。
import { applyGpuMode } from '../../components/terminal/useXterm';
import { normalizeFont, DEFAULT_FONT } from '../../lib/termfonts';
import { setLang as i18nSetLang, langChangedToast, isLang, T, terr } from '../../i18n';
import { isTauri } from '../../ipc/env';
import * as api from '../../ipc/commands';
import type { Preferences, Lang, Theme, NavStyle, GpuMode, SideTab } from '../../types/models';
import type { AppStore } from '../types';
import type { SliceCreator } from './shared';
import { setActiveBeforeX, getActiveBeforeX } from './guards';

/** 偏好保存必须串行：滑块/输入框会连续触发，并发 invoke 的落盘顺序不保证。 */
let saveChain: Promise<void> = Promise.resolve();

export const DEFAULT_PREFS: Preferences = {
  lang: 'zh-CN', theme: 'system', nav: 'classic', fontSize: 13, lineHeight: 1.3, font: DEFAULT_FONT, cursor: 'block',
  showStatus: true, showDots: true, showLineNumbers: false, confirmClose: false, restoreTabs: true, sidebarWidth: 262,
  gpuMode: 'system', autoReconnect: false, logSessions: false, keepaliveSecs: 30, requireWindowsHello: false,
  lanVisible: false, lanDeviceName: '', lanPort: 27410, peerShareRoot: '', lanStrictPairing: false,
};

/** 偏好归一化：旧配置缺字段 / 非法值一律回落默认（启动加载与备份导入共用）。 */
export function normalizePrefs(raw?: Partial<Preferences> | null): Preferences {
  const prefs: Preferences = { ...DEFAULT_PREFS, ...(raw ?? {}) };
  prefs.font = normalizeFont(prefs.font);
  // 没有 on 状态，未知值一律按 system；off 立即让终端回退 DOM
  prefs.gpuMode = prefs.gpuMode === 'off' ? 'off' : 'system';
  // 老配置文件缺这些字段时按默认 false 处理
  prefs.autoReconnect = !!prefs.autoReconnect;
  prefs.logSessions = !!prefs.logSessions;
  // 启动门禁（Windows Hello）：缺字段按默认关，任何非 true 的值都回落 false
  prefs.requireWindowsHello = !!prefs.requireWindowsHello;
  // 局域网对等层：端口钳到合法范围（0 = 自动选），文本字段去首尾空白
  prefs.lanVisible = !!prefs.lanVisible;
  prefs.lanStrictPairing = !!prefs.lanStrictPairing;
  prefs.lanDeviceName = typeof prefs.lanDeviceName === 'string' ? prefs.lanDeviceName.trim() : '';
  prefs.peerShareRoot = typeof prefs.peerShareRoot === 'string' ? prefs.peerShareRoot.trim() : '';
  prefs.lanPort = typeof prefs.lanPort === 'number' && Number.isFinite(prefs.lanPort)
    ? Math.min(65535, Math.max(0, Math.round(prefs.lanPort)))
    : DEFAULT_PREFS.lanPort;
  // 设置里没有 UI，但值必须原样透传；缺字段 / 非数字时回落 30（0 是合法值 = 禁用）
  prefs.keepaliveSecs = typeof prefs.keepaliveSecs === 'number' && Number.isFinite(prefs.keepaliveSecs)
    ? prefs.keepaliveSecs : DEFAULT_PREFS.keepaliveSecs;
  // 行高：字号倍数，限制在 UI 滑杆范围内（1.0–2.0）；缺字段 / 非数字回落默认 1.3
  prefs.lineHeight = typeof prefs.lineHeight === 'number' && Number.isFinite(prefs.lineHeight)
    ? Math.min(2, Math.max(1, Math.round(prefs.lineHeight * 100) / 100))
    : DEFAULT_PREFS.lineHeight;
  if (!isLang(prefs.lang)) prefs.lang = DEFAULT_PREFS.lang;
  return prefs;
}

export interface PrefsSlice {
  lang: Lang;
  theme: Theme; nav: NavStyle; sideTab: SideTab; sidebarWidth: number;
  fontSize: number; lineHeight: number; font: string; cursor: 'block'|'bar';
  showStatus: boolean; showDots: boolean; showLineNumbers: boolean; confirmClose: boolean; restoreTabs: boolean;
  gpuMode: GpuMode;
  autoReconnect: boolean;
  logSessions: boolean;
  keepaliveSecs: number;
  requireWindowsHello: boolean;
  lanVisible: boolean;
  lanStrictPairing: boolean;
  lanDeviceName: string;
  lanPort: number;
  peerShareRoot: string;
  collapsed: boolean; focus: boolean; search: string;
  closedGroups: Record<string, boolean>;
  /** 会话日志目录（get_app_info.logsDir 下发；打开/清空日志与隐私提示共用） */
  appLogsDir: string;

  savePrefs(): void;
  setLang(l: Lang): void;
  setTheme(t: Theme): void; toggleTheme(): void; setNav(v: NavStyle): void;
  setFontSize(v: number): void; setLineHeight(v: number): void; setFont(v: string): void; setCursor(v: 'block'|'bar'): void;
  setToggle(k: 'showStatus'|'showDots'|'showLineNumbers'|'confirmClose'|'autoReconnect'|'logSessions'|'requireWindowsHello'|'lanVisible'|'lanStrictPairing'): void;
  setLanDeviceName(v: string): void;
  setLanPort(v: number): void;
  setPeerShareRoot(v: string): void;
  setGpuMode(v: GpuMode): void;
  setSidebarWidth(v: number): void;
  toggleSidebar(): void; toggleFocus(): void; setSearch(q: string): void;
  toggleGroup(g: string): void; setSideTab(t: SideTab): void;
}


export const createPrefsSlice: SliceCreator<PrefsSlice> = (set, get) => ({
  lang: DEFAULT_PREFS.lang,
  theme: DEFAULT_PREFS.theme, nav: DEFAULT_PREFS.nav, sideTab: 'hosts', sidebarWidth: DEFAULT_PREFS.sidebarWidth,
  fontSize: DEFAULT_PREFS.fontSize, lineHeight: DEFAULT_PREFS.lineHeight, font: DEFAULT_PREFS.font, cursor: DEFAULT_PREFS.cursor,
  showStatus: true, showDots: true, showLineNumbers: false, confirmClose: false, restoreTabs: true,
  gpuMode: DEFAULT_PREFS.gpuMode,
  autoReconnect: false, logSessions: false, keepaliveSecs: DEFAULT_PREFS.keepaliveSecs,
  requireWindowsHello: false,
  lanVisible: false, lanDeviceName: '', lanPort: 27410, peerShareRoot: '', lanStrictPairing: false,
  collapsed: false, focus: false, search: '',
  closedGroups: {},
  appLogsDir: '',

  savePrefs() {
    const s = get();
    const prefs: Preferences = {
      lang: s.lang, theme: s.theme, nav: s.nav, fontSize: s.fontSize, lineHeight: s.lineHeight, font: s.font, cursor: s.cursor,
      showStatus: s.showStatus, showDots: s.showDots, showLineNumbers: s.showLineNumbers, confirmClose: s.confirmClose,
      restoreTabs: s.restoreTabs, sidebarWidth: s.sidebarWidth, gpuMode: s.gpuMode,
      autoReconnect: s.autoReconnect, logSessions: s.logSessions, keepaliveSecs: s.keepaliveSecs,
      requireWindowsHello: s.requireWindowsHello,
      lanVisible: s.lanVisible, lanDeviceName: s.lanDeviceName, lanPort: s.lanPort, peerShareRoot: s.peerShareRoot,
      lanStrictPairing: s.lanStrictPairing,
    };
    const run = () =>
      api.savePreferences(prefs).catch((e) => {
        // mock / 非 Tauri 环境保持静默；桌面版写盘失败必须提示（偏好没有保存成功）
        if (isTauri) get().showToast(T('err.saveFailed', { 0: terr((e as { code?: string })?.code, (e as { detail?: string })?.detail) }));
      });
    // 串行化：即使前一次失败也要继续执行下一次。
    saveChain = saveChain.then(run, run);
  },

  /* ---------- 语言（即时生效、不重建会话） ---------- */
  setLang(l) {
    if (!isLang(l)) return;
    i18nSetLang(l);
    try { localStorage.setItem('ruthenium.lang', l); } catch { /* ignore */ }
    set({ lang: l });
    get().savePrefs();
    get().showToast(langChangedToast(l));
  },

  /* ---------- 视图与设置 ---------- */
  setTheme(t) { set({ theme: t }); get().savePrefs(); },
  toggleTheme() { get().setTheme(get().theme === 'dark' ? 'light' : 'dark'); get().showToast(get().theme === 'dark' ? T('toast.dark') : T('toast.light')); },
  setNav(v) {
    if (v === 'x') {
      // 记住切走前的 active，切回经典侧栏时恢复它，而不是「最后一个会话」
      setActiveBeforeX(get().active);
      set({ nav: 'x', collapsed: true, active: 'vaults' });
    } else {
      const remembered = getActiveBeforeX() && get().sessions.some((x) => x.id === getActiveBeforeX())
        ? getActiveBeforeX()
        : (get().sessions.length ? get().sessions[get().sessions.length - 1].id : null);
      setActiveBeforeX(null);
      set({ nav: 'classic', collapsed: false, active: remembered });
    }
    get().closeOverlay();
    get().showToast(v === 'x' ? T('toast.navX') : T('toast.navClassic'));
  },
  setFontSize(v) { set({ fontSize: v }); get().savePrefs(); },
  setLineHeight(v) { set({ lineHeight: Math.min(2, Math.max(1, Math.round(v * 100) / 100)) }); get().savePrefs(); },
  setFont(v) { set({ font: v }); get().savePrefs(); },
  setCursor(v) { set({ cursor: v }); get().savePrefs(); },
  setGpuMode(v) {
    set({ gpuMode: v });
    // off 立即在已打开的终端上释放 WebGL 渲染器（回退 DOM）；system 只影响新建终端
    applyGpuMode(v);
    get().savePrefs();
  },
  setToggle(k) {
    const next = !get()[k];
    set({ [k]: next } as Partial<AppStore>);
    get().savePrefs();
    // 会话日志是**明文**终端内容，开启时明确告知位置与风险（不静默开启）。
    if (k === 'logSessions' && next) {
      void (async () => {
        let dir = get().appLogsDir;
        if (!dir) {
          try { dir = String((await api.getAppInfo())?.logsDir ?? ''); set({ appLogsDir: dir }); } catch { /* ignore */ }
        }
        get().showToast(T('toast.logEnabledWarn', { 0: dir || 'logs' }));
      })();
    }
  },
  setSidebarWidth(v) { set({ sidebarWidth: v }); get().savePrefs(); },
  setLanDeviceName(v) { set({ lanDeviceName: v }); get().savePrefs(); },
  setLanPort(v) { set({ lanPort: Math.min(65535, Math.max(0, Math.round(v))) }); get().savePrefs(); },
  setPeerShareRoot(v) { set({ peerShareRoot: v }); get().savePrefs(); },
  toggleSidebar() { set({ collapsed: !get().collapsed }); },
  toggleFocus() { set({ focus: !get().focus }); },
  setSearch(q) { set({ search: q }); },
  toggleGroup(g) { set({ closedGroups: { ...get().closedGroups, [g]: !get().closedGroups[g] } }); },
  setSideTab(t) { set({ sideTab: t, search: '' }); },
});
