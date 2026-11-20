// @vitest-environment jsdom
// 拖拽侧栏时把鼠标拖出窗口再松手，mouseup 不会派发到 document；
// 只在 mouseup 里清理会让 mousemove 监听与 col-resize 光标永久残留
// （之后鼠标移动会持续改写侧栏宽度）。用 window blur 兜底清理并提交。
import { describe, expect, it, vi } from 'vitest';
import { act, renderHook } from '@testing-library/react';
import { useResizable } from './useResizable';

describe('useResizable 清理', () => {
  it('blur（窗口外松手）时清理监听、复位光标并提交宽度', () => {
    document.body.innerHTML = '<aside class="sidebar"></aside>';
    const sidebar = document.querySelector('.sidebar') as HTMLElement;
    const onCommit = vi.fn();

    const { result } = renderHook(() => useResizable(onCommit));
    act(() => {
      result.current({ preventDefault() {}, clientX: 250 } as unknown as React.MouseEvent);
    });
    expect(document.body.style.cursor).toBe('col-resize');

    // 拖到窗口外 → 失焦：必须兜底清理
    act(() => {
      window.dispatchEvent(new Event('blur'));
    });

    expect(document.body.style.cursor).toBe('');
    expect(onCommit).toHaveBeenCalledWith(250);
    expect(sidebar.style.width).toBe('');

    // 清理已生效：之后的 mousemove 不应再被响应（宽度不会被继续改写）
    onCommit.mockClear();
    act(() => {
      document.dispatchEvent(new MouseEvent('mousemove', { clientX: 300 }));
    });
    expect(onCommit).not.toHaveBeenCalled();
  });

  it('正常 mouseup 会清理并提交最终宽度', () => {
    document.body.innerHTML = '<aside class="sidebar"></aside>';
    const onCommit = vi.fn();
    const { result } = renderHook(() => useResizable(onCommit));
    act(() => {
      result.current({ preventDefault() {}, clientX: 240 } as unknown as React.MouseEvent);
    });
    act(() => {
      document.dispatchEvent(new MouseEvent('mouseup', { clientX: 300 }));
    });
    expect(document.body.style.cursor).toBe('');
    expect(onCommit).toHaveBeenCalledWith(300);
  });
});
