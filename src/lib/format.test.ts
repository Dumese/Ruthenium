import { describe, it, expect } from 'vitest';
import { fmtSize, duration, hhmm, fmtMode } from './format';

describe('fmtSize', () => {
  it('null/undefined 返回占位符', () => {
    expect(fmtSize(null)).toBe('—');
    expect(fmtSize(undefined)).toBe('—');
  });
  it('字节', () => { expect(fmtSize(512)).toBe('512 B'); });
  it('KB 一位小数', () => { expect(fmtSize(1024)).toBe('1.0 KB'); expect(fmtSize(18940)).toBe('18.5 KB'); });
  it('MB 一位小数', () => { expect(fmtSize(1048576)).toBe('1.0 MB'); expect(fmtSize(4821102)).toBe('4.6 MB'); });
});

describe('duration', () => {
  it('秒', () => { expect(duration(0, 45_000)).toBe('45 秒'); });
  it('分钟', () => { expect(duration(0, 90_000)).toBe('1 分钟'); });
  it('小时:分', () => { expect(duration(0, 3 * 3600_000 + 11 * 60_000)).toBe('3:11'); });
  it('天', () => { expect(duration(0, 42 * 86400_000)).toBe('42 天'); });
});

describe('hhmm', () => {
  it('补零', () => { expect(hhmm(new Date(2026, 10, 10, 9, 7).getTime())).toBe('09:07'); });
});

describe('fmtMode（SFTP 权限列）', () => {
  it('目录与普通文件的权限位', () => {
    expect(fmtMode(0o040755, true)).toBe('drwxr-xr-x');
    expect(fmtMode(0o100644, false)).toBe('-rw-r--r--');
    expect(fmtMode(0o100600, false)).toBe('-rw-------');
    expect(fmtMode(0o120777, false)).toBe('lrwxrwxrwx');
  });
  it('没有权限信息时显示占位符', () => {
    expect(fmtMode(null, false)).toBe('—');
    expect(fmtMode(undefined, true)).toBe('—');
  });
});
