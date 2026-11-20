// 快速连接语法解析：正则、默认端口、边界与临时 Host 标记
import { describe, it, expect } from 'vitest';
import { parseQuickConnect, quickTargetLabel, buildQuickHost, QUICK_CONNECT_RE } from './quickConnect';

describe('parseQuickConnect', () => {
  it('user@host:port 三段齐全', () => {
    expect(parseQuickConnect('deploy@10.0.0.5:2222')).toEqual({ user: 'deploy', host: '10.0.0.5', port: 2222 });
  });
  it('省略端口默认 22', () => {
    expect(parseQuickConnect('deploy@web-01.example.com')).toEqual({ user: 'deploy', host: 'web-01.example.com', port: 22 });
  });
  it('省略用户名', () => {
    expect(parseQuickConnect('10.0.0.5:2200')).toEqual({ user: '', host: '10.0.0.5', port: 2200 });
  });
  it('两侧空白被裁剪', () => {
    expect(parseQuickConnect('  deploy@h1  ')).toEqual({ user: 'deploy', host: 'h1', port: 22 });
  });
  it('非法输入一律 null', () => {
    for (const bad of ['', '  ', 'bad input', 'user@', '@host', 'a@b@c', 'host:0', 'host:65536', 'host:-1', 'host:abc', 'host:22:33']) {
      expect(parseQuickConnect(bad), bad).toBeNull();
    }
  });
  it('正则与规格一致（命名分组 user/host/port）', () => {
    const m = QUICK_CONNECT_RE.exec('ops@edge:2202');
    expect(m?.groups).toEqual({ user: 'ops', host: 'edge', port: '2202' });
  });
});

describe('quickTargetLabel / buildQuickHost', () => {
  const t = { user: 'deploy', host: '10.0.0.5', port: 2222 };
  it('标签用 user@host', () => { expect(quickTargetLabel(t)).toBe('deploy@10.0.0.5'); });
  it('没有用户名时标签只用 host', () => { expect(quickTargetLabel({ ...t, user: '' })).toBe('10.0.0.5'); });

  it('临时 Host：quick- 前缀 id、persist=false、agent 认证、known=false', () => {
    const h = buildQuickHost(t, 1700000000000);
    expect(h.id).toBe('quick-1700000000000');
    expect(h.persist).toBe(false);
    expect(h.auth).toBe('agent');
    expect(h.known).toBe(false);
    expect(h.name).toBe('deploy@10.0.0.5');
    expect(h.addr).toBe('10.0.0.5');
    expect(h.port).toBe(2222);
    expect(h.initCommand).toBe('');
    expect(h.log).toBe(false);
  });
  it('未填用户名时主机名回落 root', () => {
    expect(buildQuickHost({ user: '', host: 'h1', port: 22 }, 1).user).toBe('root');
  });
});
