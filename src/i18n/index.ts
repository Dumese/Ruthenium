// i18n 运行时：T(key,vars) / setLang / {0} 插值 / 缺 key 回退 zh-CN 并 dev warn。
// 语言切换通过 subscribe 通知订阅者（组件用 useLang() 触发重渲染），不重建会话。
//
// 启动体积：默认语言 zh-CN 静态内置（T() 永远同步可用，作为兜底）；其余三种语言包改为
// `import()` 按需加载（构建时各自成为独立 chunk，不进首屏主包）。首次切换时先加载目标
// 词条再应用（加载期间界面保持当前语言，不会闪回中文）；加载失败也切到目标语言，T()
// 按「缺 key 回退 zh-CN」的既有语义回退。
import { useSyncExternalStore } from 'react';
import type { Lang } from '../types/models';
import zhCN from './locales/zh-CN';

/** 默认语言：唯一静态打包的词条表，永远是同步兜底。 */
export const DEFAULT_LANG = 'zh-CN' as const;
export const LANGS: { id: Lang; label: string }[] = [
  { id: 'zh-CN', label: '简体中文' },
  { id: 'zh-TW', label: '繁體中文' },
  { id: 'en', label: 'English' },
  { id: 'ja', label: '日本語' },
];

type Table = Record<string, string>;

/** 已装载词条：非默认语言的表在对应 chunk 加载完成后原地填充（保持对象引用）。 */
export const TABLE = {
  'zh-CN': zhCN,
  'zh-TW': {} as Table,
  en: {} as Table,
  ja: {} as Table,
} as Record<Lang, Table>;

/** 非默认语言的加载器：只在真正切换到该语言（或启动恢复非默认语言）时才请求对应 chunk。 */
const LOADERS: Record<Exclude<Lang, 'zh-CN'>, () => Promise<{ default: Table }>> = {
  'zh-TW': () => import('./locales/zh-TW'),
  en: () => import('./locales/en'),
  ja: () => import('./locales/ja'),
};

const loaded = new Set<Lang>([DEFAULT_LANG]);
const loading = new Map<Lang, Promise<void>>();

let cur: Lang = DEFAULT_LANG;
let switchSeq = 0;
const listeners = new Set<() => void>();

function notify(): void {
  listeners.forEach((fn) => { try { fn(); } catch { /* ignore */ } });
}

function applyDomLang(): void {
  if (typeof document !== 'undefined') document.documentElement.lang = cur;
}

export function getLang(): Lang { return cur; }

export function isLang(v: unknown): v is Lang {
  return v === 'zh-CN' || v === 'zh-TW' || v === 'en' || v === 'ja';
}

/** 确保指定语言包已加载（幂等；失败不缓存，允许下次重试）。 */
export function loadLang(l: Lang): Promise<void> {
  if (!isLang(l)) return Promise.resolve();
  if (loaded.has(l)) return Promise.resolve();
  let p = loading.get(l);
  if (!p) {
    p = LOADERS[l as Exclude<Lang, 'zh-CN'>]().then((mod) => {
      Object.assign(TABLE[l], mod.default);
      loaded.add(l);
      loading.delete(l);
      // 该语言就是在等它：加载完成后触发一次重渲染，把回退文案换成目标语言
      if (cur === l) notify();
    }).catch((e) => {
      loading.delete(l);
      if (import.meta.env.DEV) console.warn('[i18n] language chunk load failed:', l, e);
    });
    loading.set(l, p);
  }
  return p;
}

/** 切换语言：未加载时先加载再应用；同步 document.lang 并通知订阅者重渲染（不重建会话） */
export function setLang(l: Lang): void {
  if (!isLang(l)) l = DEFAULT_LANG;
  const seq = ++switchSeq;
  if (loaded.has(l)) {
    cur = l;
    applyDomLang();
    notify();
    return;
  }
  // 首包尚未到达：先补加载，完成后再切换（期间保持旧语言，避免空表闪字）。
  // 连点多个语言时只有最后一次生效（seq 守卫）。
  void loadLang(l).then(() => {
    if (seq !== switchSeq) return;
    cur = l;
    applyDomLang();
    notify();
  });
}

export function subscribe(fn: () => void): () => void {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

/** React 侧订阅：任何组件调用 useLang() 即可在语言切换时重渲染 */
export function useLang(): Lang {
  return useSyncExternalStore(subscribe, getLang, getLang);
}

/** 词典翻译；缺 key 回退 zh-CN，dev 下 warn。签名保持同步（全仓大量同步调用）。 */
export function T(key: string, vars?: Record<number | string, string | number>): string {
  let s = TABLE[cur][key];
  if (s == null) {
    s = TABLE[DEFAULT_LANG][key];
    if (s == null) {
      if (import.meta.env.DEV) console.warn('[i18n] missing key:', key);
      return key;
    }
  }
  if (vars) for (const p in vars) s = s.split('{' + p + '}').join(String(vars[p]));
  return s;
}

/** 错误码 → 人话（错误码人话必须走词条） */
export function terr(code: string | undefined, detail?: string): string {
  // 目标语言包未就绪时按 zh-CN 判断错误码是否存在（四份词条 key 集合一致，结果等价）
  const table = loaded.has(cur) ? TABLE[cur] : TABLE[DEFAULT_LANG];
  const k = code && table['err.' + code] != null ? 'err.' + code : 'err.unknown';
  return T(k) + (detail ? ' — ' + detail : '');
}

/** 语言切换 Toast 的四语模板：setLang 是「先加载后应用」，store 在调用后同步取文案，
 *  此时目标 chunk 可能尚未到达（T() 会回退当前语言）。这四条与 locales 的
 *  toast.langChanged 完全一致，仅用于这一个同步取值点，避免 Toast 语言滞后。 */
const LANG_CHANGED_TOAST: Record<Lang, string> = {
  'zh-CN': '界面语言已切换为 {0}',
  'zh-TW': '介面語言已切換為 {0}',
  en: 'Interface language: {0}',
  ja: '表示言語を {0} に変更しました',
};

/** 语言切换后的 Toast 文案（用新语言） */
export function langChangedToast(l: Lang): string {
  const label = LANGS.find((x) => x.id === l)?.label ?? l;
  const tpl = loaded.has(l) ? T('toast.langChanged') : (LANG_CHANGED_TOAST[l] ?? T('toast.langChanged'));
  return tpl.split('{0}').join(label);
}
