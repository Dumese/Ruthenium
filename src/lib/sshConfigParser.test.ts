import { describe, it, expect } from 'vitest';
import { parseSshConfig, sshCommand } from './sshConfigParser';

const SAMPLE = [
  '# 注释行',
  'Host web-prod-01',
  '  HostName 203.0.113.10',
  '  User ops',
  '  Port 22',
  '  IdentityFile ~/.ssh/id_ed25519',
  '',
  'Host jump hub',
  '  HostName 10.0.0.1',
  '  User root',
  '  Port 2202',
  '',
  'Host *',
  '  Compression yes',
].join('\n');

describe('parseSshConfig（前端预览，后端权威）', () => {
  it('解析 Host/HostName/User/Port/IdentityFile', () => {
    const out = parseSshConfig(SAMPLE);
    expect(out).toHaveLength(2); // web-prod-01 + jump（多模式整组一条）；Host * 整组丢弃
    expect(out[0]).toMatchObject({ name: 'web-prod-01', addr: '203.0.113.10', user: 'ops', port: 22 });
    expect(out[0].keyRef).toBe('~/.ssh/id_ed25519');
  });
  it('忽略 Host * 通配与注释', () => {
    const out = parseSshConfig(SAMPLE);
    expect(out.find((x) => x.name === '*')).toBeUndefined();
  });
  it('一行多 Host 名整组一条（name 取第一个模式，与后端一致）', () => {
    const out = parseSshConfig(SAMPLE);
    const names = out.map((x) => x.name);
    expect(names).toContain('jump');
    expect(names).not.toContain('hub'); // 后端只取 patterns[0]，不做多台拆分
    expect(out.find((x) => x.name === 'jump')).toMatchObject({ addr: '10.0.0.1', port: 2202 });
  });
  it('缺省端口 22、无匹配键安全', () => {
    const out = parseSshConfig('Host only-name');
    expect(out).toHaveLength(1);
    expect(out[0].port).toBe(22);
    expect(out[0].addr).toBe('only-name');
    expect(out[0].keyRef).toBe(''); // 默认空串，只有 IdentityFile 会写入
  });
  it('任一模式含通配符时整组丢弃（后端 :33-37 语义）', () => {
    const out = parseSshConfig(['Host web-* box', '  User ops', ''].join('\n'));
    expect(out).toHaveLength(0);
  });
  it('Match 块内的键不套用到上一个 Host（后端 :62-66 语义）', () => {
    const out = parseSshConfig(['Host a', '  HostName 10.0.0.1', '  Match host b', '  User evil', ''].join('\n'));
    expect(out).toHaveLength(1);
    expect(out[0]).toMatchObject({ name: 'a', addr: '10.0.0.1', user: '' });
  });
  it('duplicate 按 addr+port 比对：同地址不同端口不算重复', () => {
    const out = parseSshConfig(['Host a', '  HostName 10.0.0.1', '  Port 22', '', 'Host b', '  HostName 10.0.0.1', '  Port 2202', ''].join('\n'));
    expect(out).toHaveLength(2);
    expect(out[0].duplicate).toBe(false);
    expect(out[1].duplicate).toBe(false);
    const same = parseSshConfig(['Host a', '  HostName 10.0.0.1', '', 'Host b', '  HostName 10.0.0.1', ''].join('\n'));
    expect(same[1].duplicate).toBe(true);
  });
});

describe('sshCommand', () => {
  it('22 端口省略 -p', () => {
    expect(sshCommand({ user: 'deploy', addr: 'web-01.example.com', port: 22 })).toBe('ssh deploy@web-01.example.com');
  });
  it('非 22 端口带 -p', () => {
    expect(sshCommand({ user: 'dba', addr: '10.20.0.11', port: 2222 })).toBe('ssh -p 2222 dba@10.20.0.11');
  });
});
