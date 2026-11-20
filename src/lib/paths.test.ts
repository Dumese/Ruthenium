import { describe, expect, it } from 'vitest';
import { compactHomePath, defaultKeyDir, formatSidePath, stripLongPrefix } from './paths';

const HOME = 'C:\\Users\\Wayne';

describe('stripLongPrefix', () => {
  it('去掉 Windows canonicalize 的长路径前缀', () => {
    expect(stripLongPrefix('\\\\?\\C:\\Users\\Wayne\\.ssh\\id_rsa')).toBe('C:\\Users\\Wayne\\.ssh\\id_rsa');
    expect(stripLongPrefix('C:\\keys\\id_rsa')).toBe('C:\\keys\\id_rsa');
  });

  it('UNC 长路径前缀还原成 \\\\srv\\share 形式', () => {
    expect(stripLongPrefix('\\\\?\\UNC\\srv\\share\\keys\\id_rsa')).toBe('\\\\srv\\share\\keys\\id_rsa');
    // 普通 UNC（无前缀）原样返回
    expect(stripLongPrefix('\\\\srv\\share\\keys\\id_rsa')).toBe('\\\\srv\\share\\keys\\id_rsa');
  });
});

describe('compactHomePath', () => {
  it('主目录内的私钥压成 ~\\... 以便配置迁移', () => {
    expect(compactHomePath('C:\\Users\\Wayne\\.ssh\\id_ed25519', HOME)).toBe('~\\.ssh\\id_ed25519');
  });

  it('大小写不敏感（Windows 路径）', () => {
    expect(compactHomePath('c:\\users\\wayne\\.ssh\\id_rsa', HOME)).toBe('~\\.ssh\\id_rsa');
  });

  it('主目录外的路径保持绝对', () => {
    expect(compactHomePath('D:\\keys\\prod_ed25519', HOME)).toBe('D:\\keys\\prod_ed25519');
  });

  it('不误伤前缀相同但不同目录', () => {
    expect(compactHomePath('C:\\Users\\Wayne2\\id_rsa', HOME)).toBe('C:\\Users\\Wayne2\\id_rsa');
  });

  it('正斜杠输入归一为反斜杠', () => {
    expect(compactHomePath('C:/Users/Wayne/.ssh/id_rsa', HOME)).toBe('~\\.ssh\\id_rsa');
  });

  it('未知主目录时原样返回', () => {
    expect(compactHomePath('C:\\Users\\Wayne\\.ssh\\id_rsa', null)).toBe('C:\\Users\\Wayne\\.ssh\\id_rsa');
  });
});

describe('defaultKeyDir', () => {
  it('相对写法推导 .ssh 目录', () => {
    expect(defaultKeyDir('~\\.ssh\\id_rsa')).toBe('~\\.ssh');
    expect(defaultKeyDir(undefined)).toBe('~\\.ssh');
  });

  it('绝对路径取父目录', () => {
    expect(defaultKeyDir('C:\\Users\\Wayne\\Documents\\id_rsa')).toBe('C:\\Users\\Wayne\\Documents');
  });
});

// 「复制路径」：本地 `C:\Users\...`、远端 `/home/...`，两套逻辑分开
describe('formatSidePath', () => {
  it('本地用反斜杠，盘符根不重复拼分隔符', () => {
    expect(formatSidePath('local', 'C:\\Users\\Wayne', 'notes.md')).toBe('C:\\Users\\Wayne\\notes.md');
    expect(formatSidePath('local', 'D:', 'a.txt')).toBe('D:\\a.txt');
    expect(formatSidePath('local', '', 'C:\\')).toBe('C:\\');
  });

  it('远端保持 POSIX，根目录不出现双斜杠', () => {
    expect(formatSidePath('remote', '/home/deploy', 'app')).toBe('/home/deploy/app');
    expect(formatSidePath('remote', '/', 'etc')).toBe('/etc');
    expect(formatSidePath('remote', '/', '')).toBe('/');
  });
});
