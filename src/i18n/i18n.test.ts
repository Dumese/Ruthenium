// i18n 四语词条对齐与运行时行为
import { describe, it, expect, beforeEach, beforeAll } from 'vitest';
import { T, setLang, getLang, TABLE, LANGS, terr, DEFAULT_LANG, loadLang } from './index';
import zhCN from './locales/zh-CN';
import zhTW from './locales/zh-TW';
import en from './locales/en';
import ja from './locales/ja';

const TABLES = { 'zh-CN': zhCN, 'zh-TW': zhTW, 'en': en, 'ja': ja } as const;
const langs = Object.keys(TABLES) as (keyof typeof TABLES)[];

// 非默认语言改为按需 import()（首屏主包只静态打包 zh-CN）：
// 测试里需要断言 T()/TABLE 的四语行为，先按需加载其余三份词条，再走原有断言。
beforeAll(async () => {
  await Promise.all(langs.filter((l) => l !== DEFAULT_LANG).map((l) => loadLang(l as Parameters<typeof loadLang>[0])));
});

describe('i18n locales', () => {
  it('四份 locale key 集合完全一致（差集为空）', () => {
    const base = Object.keys(TABLES['zh-CN']).sort();
    for (const l of langs) {
      expect(Object.keys(TABLES[l]).sort(), l + ' key 集合应与 zh-CN 一致').toEqual(base);
    }
    expect(base.length).toBeGreaterThan(500);
  });

  it('值均为非空字符串，且语言名用母语写法', () => {
    for (const l of langs) {
      for (const [k, v] of Object.entries(TABLES[l])) {
        expect(typeof v, l + ':' + k).toBe('string');
        expect((v as string).length, l + ':' + k).toBeGreaterThan(0);
      }
    }
    expect(LANGS.map((x) => x.label)).toEqual(['简体中文', '繁體中文', 'English', '日本語']);
  });

  it('插值 {0} 生效', () => {
    setLang('zh-CN');
    expect(T('toast.imported', { 0: 3 })).toContain('3');
    expect(T('toast.imported', { 0: 3 })).not.toContain('{0}');
  });

  it('缺 key 回退 zh-CN', () => {
    setLang('en');
    expect(T('no.such.key')).toBe('no.such.key');
  });

  it('切换语言即时生效并可切回', () => {
    setLang('zh-CN');
    expect(getLang()).toBe('zh-CN');
    setLang('en');
    expect(getLang()).toBe('en');
    expect(T('side.tabHosts')).toBe('Hosts');
    setLang('ja');
    expect(T('side.tabHosts')).toBe(TABLE.ja['side.tabHosts']);
    setLang(DEFAULT_LANG);
  });

  beforeEach(() => setLang('zh-CN'));

  it('错误码人话走词条', () => {
    setLang('zh-CN');
    expect(terr('E_SSH_AUTH')).toBe(zhCN['err.E_SSH_AUTH']);
    expect(terr('E_WHATEVER')).toBe(zhCN['err.unknown']);
  });
});
