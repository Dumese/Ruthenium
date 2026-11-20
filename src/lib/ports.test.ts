// 端口输入规范化测试。
// 端口框清空（Number('') === 0）不得把主机静默变成"本地终端"（port=0 哨兵），
// 否则远程命令会实际打在本机。
import { describe, expect, it } from 'vitest';
import { DEFAULT_SSH_PORT, MAX_PORT, normalizePort } from './ports';

describe('normalizePort', () => {
  it('空串 / 纯空白 = 使用默认端口（不再变成 0）', () => {
    expect(normalizePort('')).toBe(DEFAULT_SSH_PORT);
    expect(normalizePort('   ')).toBe(DEFAULT_SSH_PORT);
    expect(normalizePort(undefined)).toBe(DEFAULT_SSH_PORT);
    expect(normalizePort(null)).toBe(DEFAULT_SSH_PORT);
  });

  it('合法的十进制端口原样保留', () => {
    expect(normalizePort('22')).toBe(22);
    expect(normalizePort(22)).toBe(22);
    expect(normalizePort(' 2222 ')).toBe(2222);
    expect(normalizePort('1')).toBe(1);
    expect(normalizePort(String(MAX_PORT))).toBe(MAX_PORT);
  });

  it('0 默认非法；只有显式允许本地终端哨兵时才保留', () => {
    expect(normalizePort('0')).toBeNull();
    expect(normalizePort(0)).toBeNull();
    expect(normalizePort('0', { allowZeroPort: true })).toBe(0);
    // 空串永远不等于 0，即使允许哨兵
    expect(normalizePort('', { allowZeroPort: true })).toBe(DEFAULT_SSH_PORT);
  });

  it('越界 / 负号 / 非十进制写法一律拒绝', () => {
    expect(normalizePort('65536')).toBeNull();
    expect(normalizePort('99999999999999999999')).toBeNull();
    expect(normalizePort('-1')).toBeNull();
    expect(normalizePort('22.5')).toBeNull();
    expect(normalizePort('1e3')).toBeNull();
    expect(normalizePort('0x16')).toBeNull();
    expect(normalizePort('22abc')).toBeNull();
    expect(normalizePort('abc')).toBeNull();
    expect(normalizePort(Number.NaN)).toBeNull();
    expect(normalizePort(Number.POSITIVE_INFINITY)).toBeNull();
  });

  it('结果恒为整数（浮点输入不会被当成合法端口）', () => {
    for (const v of ['1', '65535', '', ' 22 ']) {
      const n = normalizePort(v);
      expect(n === null || Number.isInteger(n)).toBe(true);
    }
  });
});
