// @vitest-environment jsdom
// 输入通道结束标记：屏幕仍连着但输入 SSH 通道单独断开时，前端要能退出操作模式。
import { beforeEach, describe, expect, it } from 'vitest';
import { useAppStore } from './useAppStore';

beforeEach(() => {
  useAppStore.setState({ rtt: {}, inputEnded: {}, clipboardEnded: {} });
});

describe('peer clipboard channel end', () => {
  it('records the ended channel id for the screen session', () => {
    useAppStore.getState().onClipboardEnded({ sessionId: 's1', id: 'clip-1' });
    expect(useAppStore.getState().clipboardEnded.s1).toBe('clip-1');
  });

  it('clearClipboardEnded lets the toolbar reset after the viewer handles it', () => {
    useAppStore.setState({ clipboardEnded: { s1: 'clip-1' } });
    useAppStore.getState().clearClipboardEnded('s1');
    expect(useAppStore.getState().clipboardEnded.s1).toBeUndefined();
  });
});

describe('peer input stats', () => {
  it('keeps rtt for normal pong', () => {
    useAppStore.getState().onInputStats({ sessionId: 's1', rttMs: 42 });
    expect(useAppStore.getState().rtt.s1).toBe(42);
    expect(useAppStore.getState().inputEnded.s1).toBeUndefined();
  });

  it('marks the exact input channel as ended and drops its rtt', () => {
    useAppStore.setState({ rtt: { s1: 30 } });
    useAppStore.getState().onInputStats({
      sessionId: 's1', rttMs: -1, ended: true, inputId: 'input-1',
    });
    expect(useAppStore.getState().rtt.s1).toBeUndefined();
    expect(useAppStore.getState().inputEnded.s1).toBe('input-1');
  });

  it('clearInputEnded lets a new input session start', () => {
    useAppStore.setState({ inputEnded: { s1: 'old' } });
    useAppStore.getState().clearInputEnded('s1');
    expect(useAppStore.getState().inputEnded.s1).toBeUndefined();
  });
});
