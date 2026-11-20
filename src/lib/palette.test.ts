import { describe, it, expect } from 'vitest';
import { fuzzyScore, filterPal, type PalItem } from './palette';

const items: PalItem[] = [
  { g: '主机', i: 'box', t: 'web-01', s: 'deploy@web-01.example.com · 生产环境', act: 'connect', arg: 'h1' },
  { g: '主机', i: 'box', t: 'db-primary', s: 'dba@10.20.0.11 · 生产环境', act: 'connect', arg: 'h3' },
  { g: '片段', i: 'code', t: '查看系统负载', s: 'uptime', act: 'snipinsert', arg: 'n1' },
  { g: '动作', i: 'command', t: '新建主机', s: '新建一台主机配置', act: 'palrun', arg: 'hostform' },
  { g: '动作', i: 'command', t: '折叠侧栏', s: '让终端更宽', act: 'palrun', arg: 'sidebar' },
];

describe('fuzzyScore', () => {
  it('连续命中得分高于离散命中', () => {
    expect(fuzzyScore('web-01', 'web')).toBeGreaterThan(fuzzyScore('awbxe', 'web'));
  });
  it('不匹配返回 -1', () => { expect(fuzzyScore('db-primary', 'zzz')).toBe(-1); });
});

describe('filterPal', () => {
  it('空 query 返回全部（截断到 limit）', () => { expect(filterPal(items, '')).toHaveLength(items.length); });
  it('匹配主机与片段', () => {
    const out = filterPal(items, 'web');
    expect(out.some((x) => x.t === 'web-01')).toBe(true);
    expect(out.some((x) => x.t === 'db-primary')).toBe(false);
  });
  it('匹配动作表', () => {
    expect(filterPal(items, '主机').some((x) => x.arg === 'hostform')).toBe(true);
    expect(filterPal(items, '侧栏').some((x) => x.arg === 'sidebar')).toBe(true);
  });
  it('无匹配返回空', () => { expect(filterPal(items, 'qqqq')).toHaveLength(0); });
  it('键盘选择依赖稳定排序：完全命中排前', () => {
    const out = filterPal(items, '新建主机');
    expect(out[0].arg).toBe('hostform');
  });
});
