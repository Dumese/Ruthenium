// @vitest-environment jsdom
// 前端 store 级测试：
// - 端口框清空/非法时的保存行为（绝不能把主机静默写成 port=0 本地终端）
// - 删除主机落盘失败时不得"假删除"
// - 关闭 connecting 标签会中断在途连接（cancel_connect）并收起该主机的指纹弹窗
import { beforeEach, describe, expect, it, vi } from 'vitest';

const apiMock = vi.hoisted(() => ({
  upsertHost: vi.fn(),
  deleteHost: vi.fn(),
  deletePassword: vi.fn(),
  cancelConnect: vi.fn(),
  connectSession: vi.fn(),
  clearSessionLogs: vi.fn(),
}));

vi.mock('../ipc/commands', async (importOriginal) => {
  const actual = await importOriginal<typeof import('../ipc/commands')>();
  return {
    ...actual,
    upsertHost: apiMock.upsertHost,
    deleteHost: apiMock.deleteHost,
    deletePassword: apiMock.deletePassword,
    cancelConnect: apiMock.cancelConnect,
    connectSession: apiMock.connectSession,
    clearSessionLogs: apiMock.clearSessionLogs,
  };
});

import { __resetSessionGuards, useAppStore } from './useAppStore';
import type { FormDraft, Host, Session } from '../types/models';

const makeHost = (over: Partial<Host> = {}): Host => ({
  id: 'h1', name: 'web-01', addr: '203.0.113.10', user: 'deploy', port: 22,
  group: 'prod', tag: 'prod', auth: 'key', keyRef: '~\\.ssh\\id_ed25519',
  known: false, agent: false, note: '', log: false, initCommand: '',
  ...over,
});

const makeSession = (over: Partial<Session> = {}): Session => ({
  id: 'pending-1', hostId: 'h1', kind: 'term', title: 'web-01',
  status: 'connecting', startedAt: 0, encoding: 'UTF-8', log: false,
  ...over,
});

beforeEach(() => {
  apiMock.upsertHost.mockReset();
  apiMock.deleteHost.mockReset();
  apiMock.deletePassword.mockReset();
  apiMock.cancelConnect.mockReset();
  apiMock.connectSession.mockReset();
  apiMock.clearSessionLogs.mockReset();
  // 模块级可变状态（cancelledConnects / pwRetries / retryTimers 等）
  // 必须一并重置，否则用例之间存在顺序依赖（zustand setState 覆盖不到它们）。
  __resetSessionGuards();
  useAppStore.setState({
    toast: null, form: null, hosts: [], sessions: [], active: null,
    hostkey: null, overlay: null, transientHosts: [], confirm: null,
  });
});

describe('端口规范化', () => {
  it('端口框清空 → 按默认 22 保存，绝不落成 0', async () => {
    apiMock.upsertHost.mockImplementation(async (h: Host) => h);
    useAppStore.setState({ form: { _t: 'host', name: 'web', addr: '203.0.113.10', port: '' } });
    await useAppStore.getState().saveForm();
    expect(apiMock.upsertHost).toHaveBeenCalledTimes(1);
    expect((apiMock.upsertHost.mock.calls[0][0] as Host).port).toBe(22);
  });

  it('端口非法（abc）→ 中止保存并提示', async () => {
    useAppStore.setState({ form: { _t: 'host', name: 'web', addr: '203.0.113.10', port: 'abc' } });
    await useAppStore.getState().saveForm();
    expect(apiMock.upsertHost).not.toHaveBeenCalled();
    expect(String(useAppStore.getState().toast)).toContain('端口');
  });

  it('端口显式写 0（本地终端哨兵）→ 新建主机时拒绝', async () => {
    useAppStore.setState({ form: { _t: 'host', name: 'local', addr: '127.0.0.1', port: '0' } });
    await useAppStore.getState().saveForm();
    expect(apiMock.upsertHost).not.toHaveBeenCalled();
  });
});

describe('删除失败不假删', () => {
  it('delete_host 失败 → 保留本地条目并提示删除失败', async () => {
    apiMock.deletePassword.mockResolvedValue(undefined);
    apiMock.deleteHost.mockRejectedValue({ code: 'E_CONFIG_IO', detail: 'disk full' });
    useAppStore.setState({ hosts: [makeHost()] });

    await useAppStore.getState().deleteHostById('h1');

    expect(useAppStore.getState().hosts).toHaveLength(1);
    expect(useAppStore.getState().hosts[0].id).toBe('h1');
    expect(String(useAppStore.getState().toast)).toContain('删除失败');
  });
});

describe('关闭 connecting 标签', () => {
  it('调用 cancel_connect 并收起该主机的指纹弹窗', async () => {
    apiMock.cancelConnect.mockResolvedValue(true);
    useAppStore.setState({
      sessions: [makeSession()],
      active: 'pending-1',
      hostkey: {
        sessionId: 's1', hostId: 'h1', host: '203.0.113.10', port: 22,
        algo: 'ED25519', fingerprint: 'SHA256:test', changed: false,
      },
      overlay: { kind: 'modal', id: 'hostkey' },
    });

    await useAppStore.getState().closeSession('pending-1');

    expect(apiMock.cancelConnect).toHaveBeenCalledWith('pending-1');
    expect(useAppStore.getState().hostkey).toBeNull();
    expect(useAppStore.getState().sessions).toHaveLength(0);
  });

  it('非 connecting 会话关闭时不应调用 cancel_connect', async () => {
    useAppStore.setState({ sessions: [makeSession({ id: 's2', status: 'connected' })], active: 's2' });
    await useAppStore.getState().closeSession('s2');
    expect(apiMock.cancelConnect).not.toHaveBeenCalled();
  });
});

describe('认证失败后 active 不悬空', () => {
  it('删除临时标签时把 active 一起回退（不留指向不存在会话的 id）', async () => {
    apiMock.connectSession.mockRejectedValue({
      code: 'E_SSH_AUTH',
      detail: '未保存密码：主机 web-01 使用密码认证，但系统凭据管理器中没有密码，请先输入密码',
    });
    useAppStore.setState({ hosts: [makeHost({ id: 'h2', auth: 'password' })], sessions: [], active: null });

    const ok = await useAppStore.getState().connect('h2');

    expect(ok).toBe(false);
    expect(useAppStore.getState().sessions).toHaveLength(0);
    // active 若留在已删除的 pending-xxx 上 → 主区域显示"空状态"且无标签高亮
    expect(useAppStore.getState().active).toBeNull();
  });
});

describe('会话日志清理入口', () => {
  it('「清空会话日志」走确认弹窗，确认后调用 clear_session_logs 并提示删除数量', async () => {
    apiMock.clearSessionLogs.mockResolvedValue(3);
    useAppStore.getState().requestClearLogs();
    expect(useAppStore.getState().confirm?.fn).toBe('clearlogs');

    useAppStore.getState().confirmYes();
    // 等待 clearSessionLogs 的 async 链完成
    await Promise.resolve();
    await Promise.resolve();

    expect(apiMock.clearSessionLogs).toHaveBeenCalledTimes(1);
    expect(String(useAppStore.getState().toast)).toContain('3');
  });
});

describe('known 语义与默认值', () => {  it('新建主机默认未被信任（known=false，首次连接必须校验指纹）', () => {
    useAppStore.getState().openHostForm();
    expect((useAppStore.getState().form as FormDraft).known).toBe(false);
  });

  it('编辑已信任主机时保留 known=true', () => {
    useAppStore.getState().openHostForm(makeHost({ known: true }));
    expect((useAppStore.getState().form as FormDraft).known).toBe(true);
  });
});
